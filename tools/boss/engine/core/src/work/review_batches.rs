//! Durable review-batch persistence.
//!
//! This module owns immutable batch/member creation and read APIs, plus the
//! two-of-three quorum state machine ([`try_advance_review_batch_quorum_in_tx`])
//! that decides when enough leaf reviewers have settled to dispatch the
//! consolidating supervisor, or to give up on the batch. It deliberately does
//! not apply a supervisor's verdict; that is
//! [`crate::work::proposal_apply::apply_review_verdict`]'s job.

use std::str::FromStr;

use anyhow::{Result, bail};
use boss_protocol::{
    CreateAttentionItemInput, ExecutionKind, ExecutionStatus, ReviewBatch, ReviewBatchMember, ReviewBatchMemberRole,
    ReviewBatchMemberStatus, ReviewBatchPhase, ReviewBatchStatus, ReviewClassification,
};
use rusqlite::types::Type;
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};

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
        members.push(insert_batch_member_in_tx(tx, &batch_id, input_member, &now)?);
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

/// Insert one member row and return its typed form. Shared by
/// [`create_review_batch_in_tx`] (three leaf members at batch creation) and
/// [`try_advance_review_batch_quorum_in_tx`] (the single supervisor member,
/// added later once the leaves have settled).
fn insert_batch_member_in_tx(
    tx: &Transaction<'_>,
    batch_id: &str,
    input_member: &ReviewBatchMemberCreateInput,
    now: &str,
) -> Result<ReviewBatchMember> {
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
    Ok(ReviewBatchMember::builder()
        .id(id)
        .batch_id(batch_id.to_owned())
        .attempt(input_member.attempt)
        .created_at(now.to_owned())
        .provider_effort(input_member.provider_effort.clone())
        .requested_driver(input_member.requested_driver.clone())
        .resolved_model(input_member.resolved_model.clone())
        .role(input_member.role)
        .status(input_member.status)
        .updated_at(now.to_owned())
        .maybe_execution_id(input_member.execution_id.clone())
        .build())
}

/// Look up the only batch allowed for an immutable target, against any
/// open connection or transaction. Shared by the public
/// [`WorkDb::review_batch_for_target`] and the in-transaction lookup in
/// [`WorkDb::create_pre_merge_review_batch`] so the fourteen-column SELECT
/// and `map_review_batch` stay in one place.
fn review_batch_for_target_in(
    conn: &rusqlite::Connection,
    cycle_root_id: &str,
    phase: ReviewBatchPhase,
    target_sha: &str,
) -> Result<Option<ReviewBatch>> {
    conn.query_row(
        "SELECT id, cycle_root_id, base_sha, classification_json, created_at,
                phase, pr_number, pr_url, status, target_sha, updated_at,
                completed_at, final_verdict_proposal_id, merge_sha
         FROM pr_review_batches WHERE cycle_root_id = ?1 AND phase = ?2 AND target_sha = ?3",
        params![cycle_root_id, phase.as_str(), target_sha],
        map_review_batch,
    )
    .optional()
    .map_err(Into::into)
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

/// Look up a batch by its durable id, against any open connection or
/// transaction. Shared by [`WorkDb::review_batch`] and
/// [`try_advance_review_batch_quorum_in_tx`].
fn review_batch_by_id_in(conn: &rusqlite::Connection, batch_id: &str) -> Result<Option<ReviewBatch>> {
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

/// Return every role-specific attempt for a batch, in deterministic
/// role/attempt order, against any open connection or transaction. Shared by
/// [`WorkDb::review_batch_members`] and [`try_advance_review_batch_quorum_in_tx`].
fn review_batch_members_in(conn: &rusqlite::Connection, batch_id: &str) -> Result<Vec<ReviewBatchMember>> {
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

/// The three leaf reviewer roles a batch's quorum decision is computed over.
/// `ReviewBatchMemberRole::Supervisor` and `PostMergeReviewer` are never part
/// of the leaf quorum.
const LEAF_REVIEWER_ROLES: [ReviewBatchMemberRole; 3] = [
    ReviewBatchMemberRole::ClaudeReviewer,
    ReviewBatchMemberRole::CodexReviewer,
    ReviewBatchMemberRole::GrokReviewer,
];

/// A leaf role is settled once its latest attempt either reported, or failed
/// with no further retry possible (`retry_dead_review_batch_member` bounds
/// every role to at most one retry, so `attempt >= 2` while `Failed` is
/// terminal — mirrors that function's own bound rather than introducing a
/// second one).
fn leaf_attempt_is_settled(member: &ReviewBatchMember) -> bool {
    match member.status {
        ReviewBatchMemberStatus::Reported => true,
        ReviewBatchMemberStatus::Failed => member.attempt >= 2,
        ReviewBatchMemberStatus::Pending | ReviewBatchMemberStatus::Running => false,
    }
}

/// The latest (highest-attempt) row for `role` among `members`, or `None` if
/// the role has no row at all (should not happen for a `collecting` batch,
/// but a caller must not panic on a data inconsistency).
fn latest_attempt_for_role(members: &[ReviewBatchMember], role: ReviewBatchMemberRole) -> Option<&ReviewBatchMember> {
    members.iter().filter(|m| m.role == role).max_by_key(|m| m.attempt)
}

/// The `repo_remote_url` any settled member's execution was launched with —
/// needed to spawn the supervisor's own execution, which
/// [`ReviewBatchCreateInput`] never persists on the batch row itself (only
/// [`WorkDb::create_pre_merge_review_batch`]'s caller supplies it, out of
/// band, for the three original leaf executions).
fn any_member_repo_remote_url(tx: &Transaction<'_>, members: &[ReviewBatchMember]) -> Result<String> {
    for member in members {
        let Some(execution_id) = member.execution_id.as_deref() else {
            continue;
        };
        if let Some(execution) = query_execution(tx, execution_id)? {
            return Ok(execution.repo_remote_url);
        }
    }
    bail!("review batch has no member execution to source repo_remote_url from")
}

/// What [`try_advance_review_batch_quorum_in_tx`] did.
#[derive(Debug)]
pub enum ReviewBatchQuorumOutcome {
    /// The batch's members have not all settled yet (still `collecting`
    /// leaves in flight), or the batch is already past `supervising`. Not an
    /// error — most calls to this function are speculative "did this
    /// settle?" checks that resolve to nothing yet.
    NoOp,
    /// Two or three leaves reported; the supervisor member and its execution
    /// were just created and the batch moved to `supervising`.
    SupervisorDispatched,
    /// The supervisor reported; the batch moved to `completed`.
    Completed,
    /// Either fewer than two leaves reported (both exhausted their retry), or
    /// the supervisor stopped without submitting a verdict. The batch moved
    /// to `failed` and a human-visible attention was filed — this is the
    /// "hold rather than produce a clean verdict" outcome.
    InsufficientQuorum,
}

/// The two-of-three quorum state machine for one review batch.
///
/// Idempotent and safe to call redundantly from multiple hook points (a leaf
/// report accepted, a leaf finalized failed, a leaf's retry exhausted, the
/// supervisor's verdict accepted, the supervisor finalized failed): it reads
/// the batch's current status and every member's current state fresh each
/// time, and only acts when that snapshot has just become decidable.
///
/// - `collecting`: once every leaf role has settled (reported, or failed with
///   its one retry exhausted), advance based on how many reported.
///   `>= 2` dispatches the supervisor (`supervising`); `< 2` fails the batch
///   — the "fewer than two reports must hold rather than produce a clean
///   verdict" requirement. Still-in-flight roles are a no-op.
/// - `supervising`: once the supervisor's own member has settled (reported or
///   failed — the supervisor gets no retry, unlike a leaf), advance to
///   `completed` or `failed` accordingly.
/// - Any other status (`applying`, `completed`, `failed`): no-op. This is
///   what makes redundant calls safe.
///
/// Must be called inside an already-open transaction — see
/// [`WorkDb::try_advance_review_batch_quorum`] for the standalone wrapper.
pub(crate) fn try_advance_review_batch_quorum_in_tx(
    tx: &Transaction<'_>,
    batch_id: &str,
    registry: &crate::driver::DriverRegistry,
) -> Result<ReviewBatchQuorumOutcome> {
    let Some(batch) = review_batch_by_id_in(tx, batch_id)? else {
        return Ok(ReviewBatchQuorumOutcome::NoOp);
    };
    let members = review_batch_members_in(tx, batch_id)?;
    let now = now_string();

    match batch.status {
        ReviewBatchStatus::Collecting => {
            let mut reported = 0usize;
            for role in LEAF_REVIEWER_ROLES {
                match latest_attempt_for_role(&members, role) {
                    Some(member) if leaf_attempt_is_settled(member) => {
                        if member.status == ReviewBatchMemberStatus::Reported {
                            reported += 1;
                        }
                    }
                    _ => return Ok(ReviewBatchQuorumOutcome::NoOp),
                }
            }

            if reported >= 2 {
                let repo_remote_url = any_member_repo_remote_url(tx, &members)?;
                let supervisor_driver = registry
                    .get("claude")
                    .ok_or_else(|| anyhow::anyhow!("supervisor driver \"claude\" is not registered"))?
                    .descriptor();
                let resolved_model =
                    (supervisor_driver.model_menu.review_model_for_tier)(batch.classification.profile.model_tier());
                let execution = insert_execution(
                    tx,
                    CreateExecutionInput::builder()
                        .work_item_id(batch.cycle_root_id.clone())
                        .kind(ExecutionKind::PrReview)
                        .status(ExecutionStatus::Ready)
                        .repo_remote_url(repo_remote_url)
                        .build(),
                )?;
                let member_input = ReviewBatchMemberCreateInput::builder()
                    .attempt(1)
                    .provider_effort("medium")
                    .requested_driver("claude")
                    .resolved_model(resolved_model)
                    .role(ReviewBatchMemberRole::Supervisor)
                    .status(ReviewBatchMemberStatus::Pending)
                    .execution_id(execution.id.clone())
                    .build();
                validate_member_input(batch.phase, &member_input)?;
                insert_batch_member_in_tx(tx, batch_id, &member_input, &now)?;
                tx.execute(
                    "UPDATE pr_review_batches SET status = 'supervising', updated_at = ?2 WHERE id = ?1",
                    params![batch_id, now],
                )?;
                Ok(ReviewBatchQuorumOutcome::SupervisorDispatched)
            } else {
                tx.execute(
                    "UPDATE pr_review_batches SET status = 'failed', completed_at = ?2, updated_at = ?2 WHERE id = ?1",
                    params![batch_id, now],
                )?;
                super::workitems::insert_attention_item_row(
                    tx,
                    &CreateAttentionItemInput::builder()
                        .work_item_id(batch.cycle_root_id.clone())
                        .kind("pr_review_quorum_failed")
                        .title("Automated reviewer: insufficient quorum")
                        .body_markdown(format!(
                            "Fewer than two of the three independent reviewers reported for {} \
                             (batch `{batch_id}`) — the remaining role(s) exhausted their retry \
                             without submitting a report. The review cannot produce a consolidated \
                             verdict without at least two independent reports.",
                            batch.pr_url,
                        ))
                        .build(),
                )?;
                Ok(ReviewBatchQuorumOutcome::InsufficientQuorum)
            }
        }
        ReviewBatchStatus::Supervising => match latest_attempt_for_role(&members, ReviewBatchMemberRole::Supervisor) {
            Some(member) if member.status == ReviewBatchMemberStatus::Reported => {
                let proposal_id = member
                    .report_proposal_id
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("reported supervisor member is missing its report_proposal_id"))?;
                tx.execute(
                    "UPDATE pr_review_batches
                         SET status = 'completed', completed_at = ?2, updated_at = ?2, final_verdict_proposal_id = ?3
                         WHERE id = ?1",
                    params![batch_id, now, proposal_id],
                )?;
                Ok(ReviewBatchQuorumOutcome::Completed)
            }
            Some(member) if member.status == ReviewBatchMemberStatus::Failed => {
                tx.execute(
                    "UPDATE pr_review_batches SET status = 'failed', completed_at = ?2, updated_at = ?2 WHERE id = ?1",
                    params![batch_id, now],
                )?;
                super::workitems::insert_attention_item_row(
                    tx,
                    &CreateAttentionItemInput::builder()
                        .work_item_id(batch.cycle_root_id.clone())
                        .kind("pr_review_quorum_failed")
                        .title("Automated reviewer: supervisor did not produce a verdict")
                        .body_markdown(format!(
                            "The consolidating supervisor for {} (batch `{batch_id}`) stopped \
                                 without submitting a review-verdict proposal. It is not retried \
                                 automatically; the review cannot produce a consolidated verdict.",
                            batch.pr_url,
                        ))
                        .build(),
                )?;
                Ok(ReviewBatchQuorumOutcome::InsufficientQuorum)
            }
            _ => Ok(ReviewBatchQuorumOutcome::NoOp),
        },
        ReviewBatchStatus::Applying | ReviewBatchStatus::Completed | ReviewBatchStatus::Failed => {
            Ok(ReviewBatchQuorumOutcome::NoOp)
        }
    }
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
        if let Some(batch) = review_batch_for_target_in(&tx, &input.cycle_root_id, input.phase, &input.target_sha)? {
            let executions = batch_executions_in_tx(&tx, &batch.id)?;
            tx.commit()?;
            return Ok(ReviewBatchDispatch::ExistingBatch { batch, executions });
        }
        if let Some(execution) = existing_nonterminal_pr_review_execution(&tx, &input.cycle_root_id)? {
            // `existing_nonterminal_pr_review_execution` cannot itself tell a
            // batch leaf apart from a genuine legacy single reviewer — both
            // are non-terminal `pr_review` executions on the same work item.
            // A leaf from a different (now-stale) target, or a fresh retry
            // execution inserted by `retry_dead_review_batch_member`, must
            // never be misread as "legacy reviewer owns this target": that
            // would silently suppress batch creation for the new target.
            let is_batch_leaf: Option<()> = tx
                .query_row(
                    "SELECT 1 FROM pr_review_batch_members WHERE execution_id = ?1",
                    params![execution.id],
                    |_| Ok(()),
                )
                .optional()?;
            if is_batch_leaf.is_none() {
                tx.commit()?;
                return Ok(ReviewBatchDispatch::LegacyExecution(execution));
            }
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
        review_batch_by_id_in(&conn, batch_id)
    }

    /// Look up the only batch allowed for an immutable target.
    pub fn review_batch_for_target(
        &self,
        cycle_root_id: &str,
        phase: ReviewBatchPhase,
        target_sha: &str,
    ) -> Result<Option<ReviewBatch>> {
        let conn = self.connect()?;
        review_batch_for_target_in(&conn, cycle_root_id, phase, target_sha)
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
        review_batch_members_in(&conn, batch_id)
    }

    /// Advance a batch past `collecting`/`supervising` if its members have
    /// settled enough to decide the next step. See
    /// [`try_advance_review_batch_quorum_in_tx`] for the state machine. Opens
    /// its own transaction — callers that already hold one (e.g.
    /// `apply_review_report`, `retry_dead_review_batch_member`) must call the
    /// `_in_tx` function directly instead, since `WorkDb::connect` guards a
    /// single shared connection and a nested transaction would deadlock.
    pub fn try_advance_review_batch_quorum(&self, batch_id: &str) -> Result<ReviewBatchQuorumOutcome> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let registry = crate::driver::DriverRegistry::default();
        let outcome = try_advance_review_batch_quorum_in_tx(&tx, batch_id, &registry)?;
        tx.commit()?;
        Ok(outcome)
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
    ///
    /// `member.status` includes `'failed'` alongside `'pending'`/`'running'`
    /// because `finalize_review_batch_member` already stamps a member
    /// `'failed'` (and completes its execution) when its worker stops
    /// without submitting a review-report proposal — that is the designed
    /// main path this recovery sweep exists to retry by role. Excluding
    /// `'failed'` here would leave only the narrower host-death case (a
    /// member still `'pending'`/`'running'` whose execution died without a
    /// Stop hook) ever recovered. `retry_dead_review_batch_member` already
    /// bounds this to one retry per role via `member.attempt >= 2` and the
    /// existing-retry check; this query mirrors both bounds directly (rather
    /// than relying solely on the caller's post-hoc "already exhausted its
    /// one retry" skip) so an attempt that can never be retried again is
    /// listed at most once — otherwise it would keep costing a `gh pr view`
    /// probe (see `pr_review_recovery`'s `pr_checker.check` call) on every
    /// sweep for the remaining life of the PR.
    pub fn list_dead_review_batch_member_candidates(&self) -> Result<Vec<DeadPrReviewCandidate>> {
        let conn = self.connect()?;
        let unproductive_completed = super::review_verdicts::unproductive_completed_pr_review_sql();
        let sql = format!(
            "SELECT we.work_item_id, we.id, we.status
             FROM pr_review_batch_members member
             JOIN pr_review_batches batch ON batch.id = member.batch_id
             JOIN work_executions we ON we.id = member.execution_id
             JOIN tasks task ON task.id = we.work_item_id
             WHERE batch.phase = 'pre_merge'
               AND member.role IN ('claude_reviewer', 'codex_reviewer', 'grok_reviewer')
               AND member.status IN ('pending', 'running', 'failed')
               AND member.attempt < 2
               AND NOT EXISTS (
                   SELECT 1 FROM pr_review_batch_members retry
                   WHERE retry.batch_id = member.batch_id
                     AND retry.role = member.role
                     AND retry.attempt = member.attempt + 1
               )
               AND batch.status NOT IN ('completed', 'failed')
               AND task.deleted_at IS NULL AND task.status NOT IN ('done', 'archived')
               AND (we.status IN ('orphaned', 'abandoned', 'failed', 'cancelled') OR {unproductive_completed})
             ORDER BY task.updated_at ASC, we.id ASC"
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
            // This role has now permanently settled failed (no more
            // retries) — the exact moment the quorum decision for this
            // batch can change, so check it before committing rather than
            // waiting for some other hook to notice.
            let registry = crate::driver::DriverRegistry::default();
            try_advance_review_batch_quorum_in_tx(&tx, &member.batch_id, &registry)?;
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
        insert_batch_member_in_tx(&tx, &member.batch_id, &member_input, &now)?;
        tx.commit()?;
        Ok(Some(retry))
    }
}
