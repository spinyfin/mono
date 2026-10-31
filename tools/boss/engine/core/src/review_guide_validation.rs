//! Review-guide validation above storage.
//!
//! A guide body is validated against the execution's pinned workspace: every
//! linked line range is read with `git`, which spawns processes and waits on
//! them. That must never happen while `WorkDb` holds its connection mutex or a
//! write transaction, so storage does not do it. This module owns the
//! orchestration — load the owned submission from storage, validate it here,
//! hand the verdict back — for the three paths that accept a guide:
//!
//! - live submission ([`submit_worker_proposal`]), where storage judges the
//!   verdict inside the proposal transaction after its attempt-state checks;
//! - the finalizer, which re-validates the stored body through
//!   [`validate_at_workspace`] before publishing it;
//! - startup recovery of bodies that older engines stored as refused
//!   submissions ([`recover_orphaned_submissions`]).
//!
//! The validators themselves are `boss_review_guide`'s and
//! [`crate::review_guide_workspace`]'s; nothing here reimplements them.

use anyhow::{Context, Result, anyhow};
use boss_pr_review_sources::SourcePacket;
use boss_protocol::{ProposalKind, ProposalSubmissionError};

use crate::work::{OrphanedGuideSubmission, SubmitWorkerProposalInput, SubmitWorkerProposalOutcome, WorkDb};

/// Validate a review-guide body for `execution_id` against its pinned
/// workspace, returning the validated guide.
///
/// An incremental update additionally has to carry its update section. Link
/// validation spawns `git`, so it runs off the runtime worker.
pub(crate) fn validate_at_workspace(
    db: &WorkDb,
    execution_id: &str,
    raw: &str,
    packet: &SourcePacket,
) -> Result<boss_review_guide::ValidatedGuide> {
    if db.review_guide_update_context(execution_id)?.is_some() {
        boss_review_guide::validate_update_section(raw).map_err(anyhow::Error::msg)?;
    }
    let execution = db.get_execution(execution_id)?;
    let path = execution
        .workspace_path
        .context("review-guide execution has no pinned workspace")?;
    // Every linked range spawns `git`; do not pin a runtime worker for it.
    crate::review_guide_workspace::off_runtime_worker(|| {
        crate::review_guide_workspace::validate(std::path::Path::new(&path), packet, raw)
    })
}

/// Submit a worker proposal, validating a review guide's body first.
///
/// Every other kind goes straight to storage. For [`ProposalKind::ReviewGuide`]
/// the submission is loaded and validated before the transaction opens, so a
/// large packet read and the `git` calls hold no lock; storage then refuses
/// the submission, storing nothing, if the attempt is not running or the
/// comparison changed while validation ran.
pub(crate) fn submit_worker_proposal(
    db: &WorkDb,
    input: SubmitWorkerProposalInput<'_>,
) -> Result<std::result::Result<SubmitWorkerProposalOutcome, ProposalSubmissionError>> {
    let guide = (input.kind == ProposalKind::ReviewGuide).then(|| {
        db.load_review_guide_for_validation(input.execution_id, input.payload_json)
            .map(|guide| {
                let validation = validate_at_workspace(db, input.execution_id, &guide.body_markdown, &guide.packet)
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                guide.validated(validation)
            })
    });
    db.submit_worker_proposal_with_guide(input, guide.as_ref())
}

/// What [`recover_orphaned_submissions`] did.
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

/// Startup pass: publish every guide that was submitted while its attempt was
/// wrongly closed (see `work::review_guide_recovery` for the history).
///
/// Each stored body is validated here, against the recorded comparison at the
/// execution's workspace, before storage adopts and publishes it under the
/// same fences a live submission would face. A body that no longer validates
/// is reported as skipped and nothing is written. Validation shells out to
/// `git`, so call this off the async runtime.
pub(crate) fn recover_orphaned_submissions(db: &WorkDb) -> Result<ReviewGuideSubmissionRecoveryReport> {
    let mut report = ReviewGuideSubmissionRecoveryReport::default();
    for submission in db.list_orphaned_review_guide_submissions()? {
        let proposal_id = submission.proposal_id.clone();
        match recover_one(db, &submission) {
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

/// Validate, adopt and publish one stored submission. `Ok(None)` means a fence
/// refused it and nothing was written; `Err` means it could not be validated
/// (body, comparison or workspace), and nothing was written.
fn recover_one(db: &WorkDb, submission: &OrphanedGuideSubmission) -> Result<Option<String>> {
    let payload: boss_protocol::ReviewGuideProposalPayload = serde_json::from_str(&submission.payload_json)
        .context("stored review-guide proposal payload does not parse")?;
    let capture = db
        .get_pr_review_guide_comparison_by_id(&submission.comparison_id)?
        .context("the comparison this attempt was generated from is gone")?;
    let validated = validate_at_workspace(db, &submission.execution_id, &payload.body_markdown, &capture.packet)
        .map_err(|err| anyhow!("stored guide body failed validation: {err:#}"))?;
    db.adopt_and_publish_orphaned_review_guide_submission(submission, &validated.markdown, &payload.body_markdown)
}

#[cfg(test)]
mod tests {
    use super::*;

    use boss_protocol::{ProposalErrorCode, ProposalState};

    use crate::test_support::{create_active_chore, create_product, open_db, seed_review_guide_series};

    const VALID_GUIDE: &str = "# Guide\n## Orientation\nA change.\n## Problem\n## Implementation\n## Example\n## Review\n## Evidence\nNo validation results reported; no screenshots registered.";

    /// The same guide with a linked line range in its body. The workspace the
    /// execution is pinned to does not exist, so the link cannot be read.
    fn guide_with_unreadable_link() -> String {
        format!(
            "{}\n\nSee [the helper](https://github.com/acme/widget/blob/{}/src/lib.rs#L1-L3).",
            VALID_GUIDE,
            "a".repeat(40)
        )
    }

    /// A seeded series with a bound, running guide execution pinned to
    /// `workspace`. Returns `(execution_id, comparison_id, attempt_id)`.
    fn running_guide(db: &WorkDb, workspace: &str) -> (String, String, String) {
        let product = create_product(db);
        let root = create_active_chore(db, &product, "guide validation");
        let (series, comparison) = seed_review_guide_series(db, &root);
        let attempt = db
            .create_pr_review_guide_attempt(&series, &comparison, boss_review_guide::PROMPT_VERSION)
            .unwrap();
        let execution = db.create_pr_review_guide_execution(&comparison, "acme/widget").unwrap();
        db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
            .unwrap();
        db.start_execution_run(&execution.id, "review", "mono", "lease", "ws", workspace)
            .unwrap();
        (execution.id, comparison, attempt.id)
    }

    fn submit(
        db: &WorkDb,
        execution_id: &str,
        comparison_id: &str,
        body: &str,
    ) -> Result<std::result::Result<SubmitWorkerProposalOutcome, ProposalSubmissionError>> {
        submit_worker_proposal(
            db,
            SubmitWorkerProposalInput {
                execution_id,
                work_item_id: comparison_id,
                kind: ProposalKind::ReviewGuide,
                payload_json: &serde_json::json!({ "body_markdown": body }).to_string(),
                idempotency_key: "guide",
            },
        )
    }

    fn stored_proposal_count(db: &WorkDb, execution_id: &str) -> i64 {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM worker_proposals WHERE execution_id = ?1",
                [execution_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn a_valid_guide_is_validated_then_applied_against_the_running_attempt() {
        let (_dir, db) = open_db();
        let (execution, comparison, attempt) = running_guide(&db, "/tmp/ws");

        let outcome = submit(&db, &execution, &comparison, VALID_GUIDE).unwrap().unwrap();

        assert_eq!(outcome.proposal.state, ProposalState::Applied);
        assert_eq!(outcome.proposal.applied_ref.as_deref(), Some(attempt.as_str()));
    }

    #[test]
    fn a_body_that_fails_workspace_validation_is_refused_with_the_detail_and_stores_nothing() {
        let (_dir, db) = open_db();
        let (execution, comparison, _attempt) = running_guide(&db, "/nonexistent/workspace");

        let refusal = submit(&db, &execution, &comparison, &guide_with_unreadable_link())
            .unwrap()
            .expect_err("an unreadable link must refuse the guide");

        assert_eq!(refusal.code, ProposalErrorCode::NotAccepted);
        assert!(refusal.message.contains("src/lib.rs"), "{}", refusal.message);
        assert_ne!(refusal.message, "this execution has no running review-guide attempt");
        assert!(!refusal.message.starts_with("could not load the comparison"));
        assert!(!refusal.message.contains("comparison changed during validation"));
        assert_eq!(stored_proposal_count(&db, &execution), 0);
        // The refusal is a judgement on this body, not a stored replay: a
        // corrected body under the same idempotency key is accepted.
        let accepted = submit(&db, &execution, &comparison, VALID_GUIDE).unwrap().unwrap();
        assert!(!accepted.already_submitted);
        assert_eq!(accepted.proposal.state, ProposalState::Applied);
    }

    #[test]
    fn other_proposal_kinds_are_not_validated_as_guides() {
        let (_dir, db) = open_db();
        let (execution, comparison, _attempt) = running_guide(&db, "/tmp/ws");

        let outcome = submit_worker_proposal(
            &db,
            SubmitWorkerProposalInput {
                execution_id: &execution,
                work_item_id: &comparison,
                kind: ProposalKind::Wait,
                payload_json: r#"{"reason":"waiting on CI","duration_secs":60}"#,
                idempotency_key: "wait",
            },
        )
        .unwrap();

        assert!(outcome.is_ok(), "{outcome:?}");
    }

    #[test]
    fn validation_requires_the_execution_to_be_pinned_to_a_workspace() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "guide validation");
        let (_series, comparison) = seed_review_guide_series(&db, &root);
        let execution = db.create_pr_review_guide_execution(&comparison, "acme/widget").unwrap();
        let packet = crate::test_support::review_guide_source_packet("base", "head");

        let error = validate_at_workspace(&db, &execution.id, VALID_GUIDE, &packet)
            .expect_err("an execution that was never started has no workspace");

        assert!(format!("{error:#}").contains("no pinned workspace"), "{error:#}");
    }

    #[test]
    fn recovery_leaves_a_stored_body_that_no_longer_validates_untouched() {
        let (_dir, db) = open_db();
        let (execution, comparison, attempt) = running_guide(&db, "/nonexistent/workspace");
        db.mark_execution_orphaned(&execution, "test: wrongly inferred death")
            .unwrap();
        let proposal_id = crate::work::next_id("prp");
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO worker_proposals
                 (id, execution_id, work_item_id, kind, payload_json, idempotency_key,
                  state, applied_ref, decided_by, decision_reason, decided_at, created_at)
             VALUES (?1, ?2, ?3, 'review_guide', ?4, 'stored', 'rejected', NULL, 'policy', ?5, ?6, ?6)",
                rusqlite::params![
                    proposal_id,
                    execution,
                    comparison,
                    serde_json::json!({ "body_markdown": guide_with_unreadable_link() }).to_string(),
                    "this execution has no running review-guide attempt",
                    crate::work::now_string(),
                ],
            )
            .unwrap();

        let report = recover_orphaned_submissions(&db).unwrap();

        assert!(report.published.is_empty(), "{report:?}");
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, proposal_id);
        assert!(
            report.skipped[0].1.contains("stored guide body failed validation"),
            "{:?}",
            report.skipped[0].1
        );
        let state: String = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT state FROM worker_proposals WHERE id = ?1",
                [&proposal_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "rejected", "nothing is adopted for a body that fails validation");
        let attempt_status: String = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT status FROM pr_review_guide_attempts WHERE id = ?1",
                [&attempt],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempt_status, "failed", "the attempt is not reopened");
    }
}
