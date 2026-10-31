//! Recover review guides that were submitted while their attempt was wrongly
//! closed.
//!
//! Before review-guide submissions refused before storing anything
//! ([`super::proposals`] returns [`boss_protocol::ProposalErrorCode::NotAccepted`]
//! for them now), a guide submitted for an execution whose attempt had been
//! closed by an inferred death — the orphan reap or the startup reconcile
//! acting on an `orphaned` row whose worker was in fact still running — was
//! stored as a `rejected` proposal with
//! [`super::review_guide_submission::NO_RUNNING_ATTEMPT_REASON`] and then
//! replayed as that same rejection on every retry. The finished guide sat in
//! `worker_proposals.payload_json` with nothing left to bind it to.
//!
//! The startup pass that finds those rows and publishes them as their PR's
//! guide is orchestrated above storage (`crate::review_verdict_apply_sweep::guide_validation`):
//! validating a body shells out to `git`, which storage never does. This
//! module is its storage half — [`WorkDb::list_orphaned_review_guide_submissions`]
//! names the candidates and
//! [`WorkDb::adopt_and_publish_orphaned_review_guide_submission`] takes the
//! caller's already-validated markdown and writes it under exactly the fences
//! a live submission would face: the attempt is reopened (or, when startup
//! re-adoption already reopened it, confirmed running) only when its
//! comparison is still the series' selected one and nothing else has been
//! published or started for the series
//! ([`super::review_guide_jobs::reopen_review_guide_attempt_for_readoption`]).
//! A body that no longer validates (or whose workspace is gone) never reaches
//! this module; it is logged and left untouched, and that PR's guide must be
//! regenerated manually.

use super::*;
use boss_protocol::ProposalDecider;

/// `worker_proposals.decision_reason` written when a recovered submission
/// is adopted, so the row says how it came to be `applied`.
const RECOVERED_DECISION_REASON: &str = "accepted on engine startup: the attempt had been closed by an inferred worker death that re-adoption or \
     a later restart disproved";

/// One stored guide body that could not bind to its attempt when it was
/// submitted.
#[derive(Debug, Clone)]
pub(crate) struct OrphanedGuideSubmission {
    pub(crate) proposal_id: String,
    pub(crate) execution_id: String,
    pub(crate) attempt_id: String,
    pub(crate) comparison_id: String,
    pub(crate) payload_json: String,
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Stored review-guide bodies whose submission was refused for want of a
    /// running attempt, where that attempt is still the one the series wants
    /// a guide for. Newest body per execution first.
    pub(crate) fn list_orphaned_review_guide_submissions(&self) -> Result<Vec<OrphanedGuideSubmission>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT p.id, p.execution_id, a.id, a.comparison_id, p.payload_json
             FROM worker_proposals p
             JOIN work_executions e ON e.id = p.execution_id
             JOIN pr_review_guide_attempts a ON a.execution_id = p.execution_id
             JOIN pr_review_guide_source_series s ON s.id = a.series_id
             WHERE p.kind = 'review_guide'
               AND p.state = 'rejected'
               AND p.decision_reason = ?1
               AND e.kind = 'pr_review_guide'
               AND a.status IN ('failed', 'running')
               AND a.failed_pre_start = 0
               AND s.selected_comparison_id = a.comparison_id
               AND s.request_epoch = a.request_epoch
               AND NOT EXISTS (SELECT 1 FROM pr_review_guide_versions v WHERE v.attempt_id = a.id)
               AND NOT EXISTS (
                   SELECT 1 FROM pr_review_guide_attempts o
                   WHERE o.series_id = a.series_id AND o.id != a.id
                     AND o.status NOT IN ('succeeded', 'failed', 'cancelled', 'superseded')
               )
               AND NOT EXISTS (
                   SELECT 1 FROM worker_proposals q
                   WHERE q.execution_id = p.execution_id AND q.kind = 'review_guide' AND q.state = 'applied'
               )
             ORDER BY p.execution_id, p.created_at DESC, p.id DESC",
        )?;
        let rows = stmt.query_map([super::review_guide_submission::NO_RUNNING_ATTEMPT_REASON], |row| {
            Ok(OrphanedGuideSubmission {
                proposal_id: row.get(0)?,
                execution_id: row.get(1)?,
                attempt_id: row.get(2)?,
                comparison_id: row.get(3)?,
                payload_json: row.get(4)?,
            })
        })?;
        let mut candidates: Vec<OrphanedGuideSubmission> = Vec::new();
        for row in rows {
            let row = row?;
            // The newest body per execution is the one the worker meant.
            if candidates
                .last()
                .is_some_and(|previous| previous.execution_id == row.execution_id)
            {
                continue;
            }
            candidates.push(row);
        }
        Ok(candidates)
    }

    /// Reopen the attempt, mark the stored proposal `applied` against it and
    /// publish the guide, all in one transaction. Returns the version id, or
    /// `None` when a fence refused it (the series moved on, or something else
    /// was published or started in the meantime) — nothing is written then,
    /// so a refused or failed publish never strands an `applied` proposal.
    pub(crate) fn adopt_and_publish_orphaned_review_guide_submission(
        &self,
        submission: &OrphanedGuideSubmission,
        markdown: &str,
        raw_output: &str,
    ) -> Result<Option<String>> {
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reopened = super::review_guide_jobs::reopen_review_guide_attempt_for_readoption(
            &tx,
            &submission.execution_id,
            &now,
            true,
        )?;
        if reopened.as_deref() != Some(submission.attempt_id.as_str()) {
            return Ok(None);
        }
        let adopted = tx.execute(
            "UPDATE worker_proposals
             SET state = 'applied', applied_ref = ?2, decided_by = ?3, decision_reason = ?4, decided_at = ?5
             WHERE id = ?1 AND state = 'rejected'
               AND NOT EXISTS (
                   SELECT 1 FROM worker_proposals q
                   WHERE q.execution_id = worker_proposals.execution_id AND q.kind = 'review_guide' AND q.state = 'applied'
               )",
            params![
                submission.proposal_id,
                submission.attempt_id,
                ProposalDecider::Policy.as_str(),
                RECOVERED_DECISION_REASON,
                now,
            ],
        )?;
        if adopted != 1 {
            return Ok(None);
        }
        match super::review_guide_jobs::publish_pr_review_guide_version_in_tx(
            &tx,
            &submission.attempt_id,
            markdown,
            raw_output,
            &now,
        )? {
            PublishReviewGuideOutcome::Published(version) => {
                tx.commit()?;
                Ok(Some(version.id))
            }
            other => {
                // Dropping the transaction rolls back the reopen and the
                // proposal update with it.
                tracing::info!(
                    proposal_id = %submission.proposal_id,
                    attempt_id = %submission.attempt_id,
                    ?other,
                    "review-guide recovery: the publish fence declined the guide; nothing was written",
                );
                Ok(None)
            }
        }
    }
}
