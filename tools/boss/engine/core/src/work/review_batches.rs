//! Durable review-batch persistence.
//!
//! This module owns only immutable batch/member creation and read APIs. It
//! deliberately does not schedule reviewers or apply their reports; those
//! later orchestration phases consume the contract established here.

use std::str::FromStr;

use anyhow::{Result, bail};
use boss_protocol::{
    ReviewBatch, ReviewBatchMember, ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase,
    ReviewBatchStatus, ReviewClassification,
};
use rusqlite::types::Type;
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};

use super::{WorkDb, next_id, now_string};

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
        for member in member_inputs {
            validate_member_input(input.phase, member)?;
        }

        let classification_json = serde_json::to_string(&input.classification)?;
        let batch_id = next_id("rvb");
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
            members.push(ReviewBatchMember {
                id,
                batch_id: batch_id.clone(),
                attempt: input_member.attempt,
                created_at: now.clone(),
                provider_effort: input_member.provider_effort.clone(),
                requested_driver: input_member.requested_driver.clone(),
                resolved_model: input_member.resolved_model.clone(),
                role: input_member.role,
                status: input_member.status,
                updated_at: now.clone(),
                execution_id: input_member.execution_id.clone(),
                report_proposal_id: None,
                terminal_at: None,
            });
        }
        tx.commit()?;

        Ok((
            ReviewBatch {
                id: batch_id,
                cycle_root_id: input.cycle_root_id,
                base_sha: input.base_sha,
                classification: input.classification,
                created_at: now.clone(),
                phase: input.phase,
                pr_number: input.pr_number,
                pr_url: input.pr_url,
                status: ReviewBatchStatus::Collecting,
                target_sha: input.target_sha,
                updated_at: now,
                completed_at: None,
                final_verdict_proposal_id: None,
                merge_sha: input.merge_sha,
            },
            members,
        ))
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
}
