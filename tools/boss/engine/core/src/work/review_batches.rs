//! Durable review-batch persistence.
//!
//! This module owns only immutable batch/member creation and read APIs. It
//! deliberately does not schedule reviewers or apply their reports; those
//! later orchestration phases consume the contract established here.

use std::str::FromStr;

use anyhow::{Result, bail};
use boss_protocol::{
    ExecutionKind, ExecutionStatus, ReviewBatch, ReviewBatchMember, ReviewBatchMemberRole, ReviewBatchMemberStatus,
    ReviewBatchPhase, ReviewBatchStatus, ReviewClassification,
};
use rusqlite::types::Type;
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};

use super::{
    CreateExecutionInput, DeadPrReviewCandidate, WorkDb, WorkExecution, existing_nonterminal_pr_review_execution,
    insert_execution, next_id, now_string, query_execution,
};

/// Inputs used to freeze an immutable review target before any member starts.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewBatchCreateInput {
    pub cycle_root_id: String,
    pub base_sha: String,
    pub classification: ReviewClassification,
    pub phase: ReviewBatchPhase,
    pub pr_number: i64,
    pub pr_url: String,
    pub target_sha: String,
    pub merge_sha: Option<String>,
}

/// Inputs for one role-specific member attempt in a new review batch.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewBatchMemberCreateInput {
    pub attempt: i64,
    pub provider_effort: String,
    pub requested_driver: String,
    pub resolved_model: String,
    pub role: ReviewBatchMemberRole,
    pub status: ReviewBatchMemberStatus,
    pub execution_id: Option<String>,
}

/// Result of atomically dispatching the leaf reviewers for one immutable PR
/// target. A legacy execution is returned rather than replaced so a flag flip
/// cannot make old- and batch-mode finalizers compete for the same review.
#[derive(Debug)]
pub enum ReviewBatchDispatch {
    Created {
        batch: ReviewBatch,
        executions: Vec<WorkExecution>,
    },
    ExistingBatch {
        batch: ReviewBatch,
        executions: Vec<WorkExecution>,
    },
    LegacyExecution(WorkExecution),
}

fn conversion_error(index: usize, error: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
}

fn parse_discriminator<T>(row: &Row<'_>, index: usize) -> rusqlite::Result<T>
where
    T: FromStr<Err = String>,
{
    let value: String = row.get(index)?;
    value
        .parse()
        .map_err(|error: String| conversion_error(index, std::io::Error::new(std::io::ErrorKind::InvalidData, error)))
}

fn parse_classification(row: &Row<'_>, index: usize) -> rusqlite::Result<ReviewClassification> {
    let value: String = row.get(index)?;
    serde_json::from_str(&value).map_err(|error| conversion_error(index, error))
}

fn map_review_batch(row: &Row<'_>) -> rusqlite::Result<ReviewBatch> {
    Ok(ReviewBatch {
        id: row.get(0)?,
        cycle_root_id: row.get(1)?,
        base_sha: row.get(2)?,
        classification: parse_classification(row, 3)?,
        created_at: row.get(4)?,
        phase: parse_discriminator(row, 5)?,
        pr_number: row.get(6)?,
        pr_url: row.get(7)?,
        status: parse_discriminator(row, 8)?,
        target_sha: row.get(9)?,
        updated_at: row.get(10)?,
        completed_at: row.get(11)?,
        final_verdict_proposal_id: row.get(12)?,
        merge_sha: row.get(13)?,
    })
}

fn map_review_batch_member(row: &Row<'_>) -> rusqlite::Result<ReviewBatchMember> {
    Ok(ReviewBatchMember {
        id: row.get(0)?,
        batch_id: row.get(1)?,
        attempt: row.get(2)?,
        created_at: row.get(3)?,
        provider_effort: row.get(4)?,
        requested_driver: row.get(5)?,
        resolved_model: row.get(6)?,
        role: parse_discriminator(row, 7)?,
        status: parse_discriminator(row, 8)?,
        updated_at: row.get(9)?,
        execution_id: row.get(10)?,
        report_proposal_id: row.get(11)?,
        terminal_at: row.get(12)?,
    })
}

fn member_role_is_valid_for_phase(phase: ReviewBatchPhase, role: ReviewBatchMemberRole) -> bool {
    match phase {
        ReviewBatchPhase::PreMerge => matches!(
            role,
            ReviewBatchMemberRole::ClaudeReviewer
                | ReviewBatchMemberRole::CodexReviewer
                | ReviewBatchMemberRole::GrokReviewer
                | ReviewBatchMemberRole::Supervisor
        ),
        ReviewBatchPhase::PostMerge => matches!(role, ReviewBatchMemberRole::PostMergeReviewer),
    }
}

fn validate_member_input(phase: ReviewBatchPhase, member: &ReviewBatchMemberCreateInput) -> Result<()> {
    if member.attempt < 1 {
        bail!("review batch member attempts start at one");
    }
    if !member_role_is_valid_for_phase(phase, member.role) {
        bail!("review member role {} is invalid for {} batch", member.role, phase);
    }
    if member.requested_driver.is_empty() || member.resolved_model.is_empty() || member.provider_effort.is_empty() {
        bail!("review batch member driver, model, and effort must be non-empty");
    }
    Ok(())
}

fn leaf_reviewer_role(role: ReviewBatchMemberRole) -> bool {
    matches!(
        role,
        ReviewBatchMemberRole::ClaudeReviewer
            | ReviewBatchMemberRole::CodexReviewer
            | ReviewBatchMemberRole::GrokReviewer
    )
}

fn leaf_member_inputs(
    classification: &ReviewClassification,
    execution_ids: &[String],
) -> Result<Vec<ReviewBatchMemberCreateInput>> {
    let roles = [
        (ReviewBatchMemberRole::ClaudeReviewer, "claude"),
        (ReviewBatchMemberRole::CodexReviewer, "codex"),
        (ReviewBatchMemberRole::GrokReviewer, "grok"),
    ];
    if execution_ids.len() != roles.len() {
        bail!("review batch dispatch requires exactly three leaf execution ids");
    }
    let registry = crate::driver::DriverRegistry::default();
    roles
        .into_iter()
        .zip(execution_ids)
        .map(|((role, driver), execution_id)| {
            let driver_descriptor = registry
                .get(driver)
                .ok_or_else(|| anyhow::anyhow!("review batch driver {driver:?} is not registered"))?
                .descriptor();
            Ok(ReviewBatchMemberCreateInput::builder()
                .attempt(1)
                .provider_effort("medium")
                .requested_driver(driver)
                .resolved_model((driver_descriptor.model_menu.review_model_for_tier)(
                    classification.profile.model_tier(),
                ))
                .role(role)
                .status(ReviewBatchMemberStatus::Pending)
                .execution_id(execution_id.clone())
                .build())
        })
        .collect()
}

fn validate_batch_input(input: &ReviewBatchCreateInput, member_inputs: &[ReviewBatchMemberCreateInput]) -> Result<()> {
    if member_inputs.is_empty() {
        bail!("review batches require at least one member");
    }
    match input.phase {
        ReviewBatchPhase::PreMerge if input.merge_sha.is_some() => {
            bail!("pre_merge review batches must not include a merge SHA");
        }
        ReviewBatchPhase::PostMerge if input.merge_sha.is_none() => {
            bail!("post_merge review batches require a merge SHA");
        }
        _ => {}
    }
    for member in member_inputs {
        validate_member_input(input.phase, member)?;
    }
    Ok(())
}

fn create_review_batch_in_tx(
    tx: &rusqlite::Transaction<'_>,
    input: ReviewBatchCreateInput,
    member_inputs: &[ReviewBatchMemberCreateInput],
) -> Result<(ReviewBatch, Vec<ReviewBatchMember>)> {
    validate_batch_input(&input, member_inputs)?;
    let classification_json = serde_json::to_string(&input.classification)?;
    let batch_id = next_id("rvb");
    let now = now_string();
    tx.execute(
        "INSERT INTO pr_review_batches (
            id, cycle_root_id, base_sha, classification_json, created_at,
            phase, pr_number, pr_url, status, target_sha, updated_at,
            completed_at, final_verdict_proposal_id, merge_sha
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'collecting', ?9, ?5, NULL, NULL, ?10)",
        params![
            batch_id,
            input.cycle_root_id,
            input.base_sha,
            classification_json,
            now,
            input.phase.as_str(),
            input.pr_number,
            input.pr_url,
            input.target_sha,
            input.merge_sha,
        ],
    )?;

    let mut members = Vec::with_capacity(member_inputs.len());
    for input_member in member_inputs {
        let id = next_id("rvm");
        tx.execute(
            "INSERT INTO pr_review_batch_members (
                id, batch_id, attempt, created_at, provider_effort,
                requested_driver, resolved_model, role, status, updated_at,
                execution_id, report_proposal_id, terminal_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?4, ?10, NULL, NULL)",
            params![
                id,
                batch_id,
                input_member.attempt,
                now,
                input_member.provider_effort,
                input_member.requested_driver,
                input_member.resolved_model,
                input_member.role.as_str(),
                input_member.status.as_str(),
                input_member.execution_id,
            ],
        )?;
        members.push(
            ReviewBatchMember::builder()
                .id(id)
                .batch_id(batch_id.clone())
                .attempt(input_member.attempt)
                .created_at(now.clone())
                .provider_effort(input_member.provider_effort.clone())
                .requested_driver(input_member.requested_driver.clone())
                .resolved_model(input_member.resolved_model.clone())
                .role(input_member.role)
                .status(input_member.status)
                .updated_at(now.clone())
                .maybe_execution_id(input_member.execution_id.clone())
                .build(),
        );
    }

    Ok((
        ReviewBatch::builder()
            .id(batch_id)
            .cycle_root_id(input.cycle_root_id)
            .base_sha(input.base_sha)
            .classification(input.classification)
            .created_at(now.clone())
            .phase(input.phase)
            .pr_number(input.pr_number)
            .pr_url(input.pr_url)
            .status(ReviewBatchStatus::Collecting)
            .target_sha(input.target_sha)
            .updated_at(now)
            .maybe_merge_sha(input.merge_sha)
            .build(),
        members,
    ))
}

fn batch_executions_in_tx(tx: &rusqlite::Transaction<'_>, batch_id: &str) -> Result<Vec<WorkExecution>> {
    let mut statement = tx.prepare(
        "SELECT execution_id FROM pr_review_batch_members WHERE batch_id = ?1 ORDER BY role ASC, attempt ASC, id ASC",
    )?;
    let execution_ids = statement
        .query_map(params![batch_id], |row| row.get::<_, Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    execution_ids
        .into_iter()
        .map(|execution_id| {
            let execution_id = execution_id
                .ok_or_else(|| anyhow::anyhow!("review batch {batch_id} has a member without an execution"))?;
            query_execution(tx, &execution_id)?
                .ok_or_else(|| anyhow::anyhow!("review batch {batch_id} references missing execution {execution_id}"))
        })
        .collect()
}

impl WorkDb {
    /// Atomically create a review batch and all of its initial member attempts.
    ///
    /// SQLite enforces one immutable target per `(cycle_root_id, phase,
    /// target_sha)` and one attempt per `(batch_id, role, attempt)`. The
    /// in-process validation adds the phase/role contract that a row-local
    /// SQL constraint cannot express.
    pub fn create_review_batch(
        &self,
        input: ReviewBatchCreateInput,
        member_inputs: &[ReviewBatchMemberCreateInput],
    ) -> Result<(ReviewBatch, Vec<ReviewBatchMember>)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let created = create_review_batch_in_tx(&tx, input, member_inputs)?;
        tx.commit()?;
        Ok(created)
    }

    /// Atomically create an immutable pre-merge batch and the three ready leaf
    /// executions. The durable member policy, rather than mutable task driver
    /// or effort settings, controls every later spawn and retry.
    pub fn create_pre_merge_review_batch(
        &self,
        input: ReviewBatchCreateInput,
        repo_remote_url: &str,
    ) -> Result<ReviewBatchDispatch> {
        if input.phase != ReviewBatchPhase::PreMerge {
            bail!("leaf reviewer dispatch only supports pre_merge batches");
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(batch) = tx
            .query_row(
                "SELECT id, cycle_root_id, base_sha, classification_json, created_at,
                        phase, pr_number, pr_url, status, target_sha, updated_at,
                        completed_at, final_verdict_proposal_id, merge_sha
                 FROM pr_review_batches WHERE cycle_root_id = ?1 AND phase = ?2 AND target_sha = ?3",
                params![input.cycle_root_id, input.phase.as_str(), input.target_sha],
                map_review_batch,
            )
            .optional()?
        {
            let executions = batch_executions_in_tx(&tx, &batch.id)?;
            tx.commit()?;
            return Ok(ReviewBatchDispatch::ExistingBatch { batch, executions });
        }
        if let Some(execution) = existing_nonterminal_pr_review_execution(&tx, &input.cycle_root_id)? {
            tx.commit()?;
            return Ok(ReviewBatchDispatch::LegacyExecution(execution));
        }
        let executions = (0..3)
            .map(|_| {
                insert_execution(
                    &tx,
                    CreateExecutionInput::builder()
                        .work_item_id(input.cycle_root_id.clone())
                        .kind(ExecutionKind::PrReview)
                        .status(ExecutionStatus::Ready)
                        .repo_remote_url(repo_remote_url)
                        .build(),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let execution_ids = executions
            .iter()
            .map(|execution| execution.id.clone())
            .collect::<Vec<_>>();
        let members = leaf_member_inputs(&input.classification, &execution_ids)?;
        let (batch, _) = create_review_batch_in_tx(&tx, input, &members)?;
        tx.commit()?;
        Ok(ReviewBatchDispatch::Created { batch, executions })
    }

    /// Look up a batch by its durable id.
    pub fn review_batch(&self, batch_id: &str) -> Result<Option<ReviewBatch>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, cycle_root_id, base_sha, classification_json, created_at,
                    phase, pr_number, pr_url, status, target_sha, updated_at,
                    completed_at, final_verdict_proposal_id, merge_sha
             FROM pr_review_batches WHERE id = ?1",
            params![batch_id],
            map_review_batch,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Look up the only batch allowed for an immutable target.
    pub fn review_batch_for_target(
        &self,
        cycle_root_id: &str,
        phase: ReviewBatchPhase,
        target_sha: &str,
    ) -> Result<Option<ReviewBatch>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, cycle_root_id, base_sha, classification_json, created_at,
                    phase, pr_number, pr_url, status, target_sha, updated_at,
                    completed_at, final_verdict_proposal_id, merge_sha
             FROM pr_review_batches
             WHERE cycle_root_id = ?1 AND phase = ?2 AND target_sha = ?3",
            params![cycle_root_id, phase.as_str(), target_sha],
            map_review_batch,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Return all persisted batches for a review cycle root, newest first.
    pub fn review_batches_for_cycle_root(&self, cycle_root_id: &str) -> Result<Vec<ReviewBatch>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT id, cycle_root_id, base_sha, classification_json, created_at,
                    phase, pr_number, pr_url, status, target_sha, updated_at,
                    completed_at, final_verdict_proposal_id, merge_sha
             FROM pr_review_batches
             WHERE cycle_root_id = ?1
             ORDER BY created_at DESC, id DESC",
        )?;
        Ok(statement
            .query_map(params![cycle_root_id], map_review_batch)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Return every role-specific attempt in deterministic role/attempt order.
    pub fn review_batch_members(&self, batch_id: &str) -> Result<Vec<ReviewBatchMember>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT id, batch_id, attempt, created_at, provider_effort,
                    requested_driver, resolved_model, role, status, updated_at,
                    execution_id, report_proposal_id, terminal_at
             FROM pr_review_batch_members
             WHERE batch_id = ?1
             ORDER BY role ASC, attempt ASC, id ASC",
        )?;
        Ok(statement
            .query_map(params![batch_id], map_review_batch_member)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Return the member that owns an execution, when that execution belongs
    /// to a persisted batch.
    pub fn review_batch_member_for_execution(&self, execution_id: &str) -> Result<Option<ReviewBatchMember>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, batch_id, attempt, created_at, provider_effort,
                    requested_driver, resolved_model, role, status, updated_at,
                    execution_id, report_proposal_id, terminal_at
             FROM pr_review_batch_members
             WHERE execution_id = ?1",
            params![execution_id],
            map_review_batch_member,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Record a batch member that stopped without submitting its required
    /// review-report proposal. This is intentionally a member failure rather
    /// than a transcript-recovery attempt: the proposal ledger is the only
    /// authoritative report-delivery channel for batch reviews.
    ///
    /// Returns `true` when a pending/running member transitioned to failed;
    /// a previously reported or failed member is left untouched.
    pub fn fail_review_batch_member_for_execution(&self, execution_id: &str) -> Result<bool> {
        let now = now_string();
        let conn = self.connect()?;
        let changed = conn.execute(
            "UPDATE pr_review_batch_members
             SET status = ?1, terminal_at = ?2, updated_at = ?2
             WHERE execution_id = ?3 AND status IN ('pending', 'running')",
            params![ReviewBatchMemberStatus::Failed.as_str(), now, execution_id],
        )?;
        Ok(changed > 0)
    }

    /// True only when two executions are read-only leaf roles of the same
    /// persisted pre-merge batch. This narrowly permits fan-out while keeping
    /// the ordinary single-writer chain guard intact.
    pub fn are_same_review_batch_leaves(&self, execution_id: &str, other_execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let found = conn
            .query_row(
                "SELECT 1
                 FROM pr_review_batch_members current
                 JOIN pr_review_batch_members other ON other.batch_id = current.batch_id
                 JOIN pr_review_batches batch ON batch.id = current.batch_id
                 WHERE current.execution_id = ?1 AND other.execution_id = ?2
                   AND batch.phase = 'pre_merge'
                   AND current.role IN ('claude_reviewer', 'codex_reviewer', 'grok_reviewer')
                   AND other.role IN ('claude_reviewer', 'codex_reviewer', 'grok_reviewer')",
                params![execution_id, other_execution_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// List each failed-in-place leaf independently. Unlike the legacy
    /// candidate query, this is not collapsed to one row per work item: three
    /// roles may validly fail or recover in parallel.
    pub fn list_dead_review_batch_member_candidates(&self) -> Result<Vec<DeadPrReviewCandidate>> {
        let conn = self.connect()?;
        let unproductive_completed =
            super::review_verdicts::unproductive_completed_pr_review_sql().replace("we.", "execution.");
        let sql = format!(
            "SELECT execution.work_item_id, execution.id, execution.status
             FROM pr_review_batch_members member
             JOIN pr_review_batches batch ON batch.id = member.batch_id
             JOIN work_executions execution ON execution.id = member.execution_id
             JOIN tasks task ON task.id = execution.work_item_id
             WHERE batch.phase = 'pre_merge'
               AND member.role IN ('claude_reviewer', 'codex_reviewer', 'grok_reviewer')
               AND member.status IN ('pending', 'running')
               AND task.deleted_at IS NULL AND task.status NOT IN ('done', 'archived')
               AND (execution.status IN ('orphaned', 'abandoned', 'failed', 'cancelled') OR {unproductive_completed})
             ORDER BY task.updated_at ASC, execution.id ASC"
        );
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map([], |row| {
            let status: String = row.get(2)?;
            Ok(DeadPrReviewCandidate {
                work_item_id: row.get(0)?,
                execution_id: row.get(1)?,
                execution_status: status
                    .parse()
                    .map_err(|error: String| rusqlite::Error::FromSqlConversionFailure(2, Type::Text, error.into()))?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Mark a dead leaf attempt failed and create its one role-scoped retry.
    /// Attempts at two are terminal for this recovery path; no sibling role
    /// is touched and no legacy single-review execution is ever inserted.
    pub fn retry_dead_review_batch_member(&self, execution_id: &str) -> Result<Option<WorkExecution>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let member = tx
            .query_row(
                "SELECT id, batch_id, attempt, created_at, provider_effort,
                        requested_driver, resolved_model, role, status, updated_at,
                        execution_id, report_proposal_id, terminal_at
                 FROM pr_review_batch_members WHERE execution_id = ?1",
                params![execution_id],
                map_review_batch_member,
            )
            .optional()?;
        let Some(member) = member else {
            tx.commit()?;
            return Ok(None);
        };
        if !leaf_reviewer_role(member.role) {
            tx.commit()?;
            return Ok(None);
        }
        let now = now_string();
        tx.execute(
            "UPDATE pr_review_batch_members SET status = 'failed', terminal_at = ?1, updated_at = ?1
             WHERE id = ?2 AND status IN ('pending', 'running')",
            params![now, member.id],
        )?;
        if member.attempt >= 2 {
            tx.commit()?;
            return Ok(None);
        }
        let retry_exists: Option<()> = tx
            .query_row(
                "SELECT 1 FROM pr_review_batch_members WHERE batch_id = ?1 AND role = ?2 AND attempt = ?3",
                params![member.batch_id, member.role.as_str(), member.attempt + 1],
                |_| Ok(()),
            )
            .optional()?;
        if retry_exists.is_some() {
            tx.commit()?;
            return Ok(None);
        }
        let dead = query_execution(&tx, execution_id)?
            .ok_or_else(|| anyhow::anyhow!("review batch member references missing execution {execution_id}"))?;
        let retry = insert_execution(
            &tx,
            CreateExecutionInput::builder()
                .work_item_id(dead.work_item_id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .repo_remote_url(dead.repo_remote_url)
                .build(),
        )?;
        let member_input = ReviewBatchMemberCreateInput::builder()
            .attempt(member.attempt + 1)
            .provider_effort(member.provider_effort)
            .requested_driver(member.requested_driver)
            .resolved_model(member.resolved_model)
            .role(member.role)
            .status(ReviewBatchMemberStatus::Pending)
            .execution_id(retry.id.clone())
            .build();
        let id = next_id("rvm");
        tx.execute(
            "INSERT INTO pr_review_batch_members (
                id, batch_id, attempt, created_at, provider_effort,
                requested_driver, resolved_model, role, status, updated_at,
                execution_id, report_proposal_id, terminal_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?4, ?9, NULL, NULL)",
            params![
                id,
                member.batch_id,
                member_input.attempt,
                now,
                member_input.provider_effort,
                member_input.requested_driver,
                member_input.resolved_model,
                member_input.role.as_str(),
                member_input.execution_id,
            ],
        )?;
        tx.commit()?;
        Ok(Some(retry))
    }
}
