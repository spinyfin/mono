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
//! This startup pass finds those rows and publishes them as their PR's guide,
//! under exactly the fences a live submission would face: the attempt is
//! reopened only when its comparison is still the series' selected one and
//! nothing else has been published or started for the series
//! ([`super::review_guide_jobs::reopen_review_guide_attempt_for_readoption`]),
//! and the body is validated against the recorded comparison at the
//! execution's workspace before anything is written. A body that no longer
//! validates (or whose workspace is gone) is logged and left untouched; the
//! operator can regenerate that PR's guide by hand.

use super::*;
use boss_protocol::ProposalDecider;

/// `worker_proposals.decision_reason` written when a recovered submission
/// is adopted, so the row says how it came to be `applied`.
const RECOVERED_DECISION_REASON: &str = "accepted on engine startup: the attempt had been closed by an inferred worker death that re-adoption or \
     a later restart disproved";

/// One stored guide body that could not bind to its attempt when it was
/// submitted.
#[derive(Debug, Clone)]
struct OrphanedGuideSubmission {
    proposal_id: String,
    execution_id: String,
    attempt_id: String,
    comparison_id: String,
    payload_json: String,
}

/// What [`WorkDb::recover_orphaned_review_guide_submissions`] did.
#[derive(Debug, Default)]
pub(crate) struct ReviewGuideSubmissionRecoveryReport {
    /// `(proposal_id, attempt_id, version_id)` for every guide published.
    pub published: Vec<(String, String, String)>,
    /// `(proposal_id, reason)` for every candidate left untouched.
    pub skipped: Vec<(String, String)>,
}

impl ReviewGuideSubmissionRecoveryReport {
    pub(crate) fn is_empty(&self) -> bool {
        self.published.is_empty() && self.skipped.is_empty()
    }
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Stored review-guide bodies whose submission was refused for want of a
    /// running attempt, where that attempt is still the one the series wants
    /// a guide for. Newest body per execution first.
    fn list_orphaned_review_guide_submissions(&self) -> Result<Vec<OrphanedGuideSubmission>> {
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
               AND a.status = 'failed'
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

    /// Reopen the attempt and mark the stored proposal `applied` against it,
    /// atomically. Returns `false` when the attempt could not be reopened
    /// (the series moved on, or something else was published or started
    /// in the meantime) — nothing is written then.
    fn adopt_orphaned_review_guide_submission(&self, submission: &OrphanedGuideSubmission) -> Result<bool> {
        let now = now_string();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reopened =
            super::review_guide_jobs::reopen_review_guide_attempt_for_readoption(&tx, &submission.execution_id, &now)?;
        if reopened.as_deref() != Some(submission.attempt_id.as_str()) {
            return Ok(false);
        }
        let adopted = tx.execute(
            "UPDATE worker_proposals
             SET state = 'applied', applied_ref = ?2, decided_by = ?3, decision_reason = ?4, decided_at = ?5
             WHERE id = ?1 AND state = 'rejected'",
            params![
                submission.proposal_id,
                submission.attempt_id,
                ProposalDecider::Policy.as_str(),
                RECOVERED_DECISION_REASON,
                now,
            ],
        )?;
        if adopted != 1 {
            return Ok(false);
        }
        tx.commit()?;
        Ok(true)
    }

    /// Startup pass: publish every guide that was submitted while its attempt
    /// was wrongly closed (see the module doc). Validation shells out to
    /// `git`, so call this off the async runtime.
    pub(crate) fn recover_orphaned_review_guide_submissions(&self) -> Result<ReviewGuideSubmissionRecoveryReport> {
        let mut report = ReviewGuideSubmissionRecoveryReport::default();
        for submission in self.list_orphaned_review_guide_submissions()? {
            let proposal_id = submission.proposal_id.clone();
            match self.recover_one_orphaned_review_guide_submission(&submission) {
                Ok(Some(version_id)) => {
                    tracing::warn!(
                        proposal_id = %submission.proposal_id,
                        execution_id = %submission.execution_id,
                        attempt_id = %submission.attempt_id,
                        version_id = %version_id,
                        "review-guide recovery: published a guide whose submission had been refused for a wrongly \
                         closed attempt",
                    );
                    report
                        .published
                        .push((proposal_id, submission.attempt_id.clone(), version_id));
                }
                Ok(None) => {
                    report.skipped.push((
                        proposal_id,
                        "the attempt could not be reopened or the publish was fenced".to_owned(),
                    ));
                }
                Err(err) => {
                    let reason = format!("{err:#}");
                    tracing::warn!(
                        proposal_id = %submission.proposal_id,
                        execution_id = %submission.execution_id,
                        attempt_id = %submission.attempt_id,
                        reason = %reason,
                        "review-guide recovery: left a stored guide submission untouched",
                    );
                    report.skipped.push((proposal_id, reason));
                }
            }
        }
        Ok(report)
    }

    /// Validate, adopt and publish one stored submission. `Ok(None)` means
    /// the adopt or publish fence refused it; `Err` means it could not be
    /// validated (body, comparison or workspace), and nothing was written.
    fn recover_one_orphaned_review_guide_submission(
        &self,
        submission: &OrphanedGuideSubmission,
    ) -> Result<Option<String>> {
        let payload: boss_protocol::ReviewGuideProposalPayload = serde_json::from_str(&submission.payload_json)
            .context("stored review-guide proposal payload does not parse")?;
        let capture = self
            .get_pr_review_guide_comparison_by_id(&submission.comparison_id)?
            .context("the comparison this attempt was generated from is gone")?;
        let validated = self
            .validate_review_guide_at_workspace(&submission.execution_id, &payload.body_markdown, &capture.packet)
            .map_err(|err| anyhow::anyhow!("stored guide body failed validation: {err:#}"))?;
        if !self.adopt_orphaned_review_guide_submission(submission)? {
            return Ok(None);
        }
        match self.publish_pr_review_guide_version(
            &submission.attempt_id,
            &validated.markdown,
            &payload.body_markdown,
        )? {
            PublishReviewGuideOutcome::Published(version) => Ok(Some(version.id)),
            other => {
                tracing::info!(
                    proposal_id = %submission.proposal_id,
                    attempt_id = %submission.attempt_id,
                    ?other,
                    "review-guide recovery: attempt reopened but the publish fence declined it",
                );
                Ok(None)
            }
        }
    }
}
