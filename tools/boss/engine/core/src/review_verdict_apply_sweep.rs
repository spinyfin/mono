//! Periodic reconciler for staged `review_verdict` proposals.
//!
//! Submission leaves a verdict `proposed` so GitHub probes and remediation
//! task creation do not block the worker socket. This sweep (and the
//! post-submit kick in `app::proposals`) applies those proposals: one
//! verdict/cycle update per batch, a revision while the origin PR is open,
//! or a follow-up against `main` if it has merged.

use std::sync::Arc;
use std::time::Duration;

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::DispatchEventSink;
use crate::work::{
    GhPrStateChecker, MergeParentComparison, PrStateChecker, ReviewVerdictApplyStats, ReviewVerdictObservation, WorkDb,
};
use anyhow::Result;
use boss_protocol::{ProposalState, WorkerProposal};

/// Observe the comparison above storage, before applying its owned result.
pub(crate) fn apply_review_verdict_proposal(
    db: &WorkDb,
    proposal_id: &str,
    pr_checker: &dyn PrStateChecker,
    observe: &dyn Fn(&MergeParentComparison) -> Vec<String>,
) -> anyhow::Result<Option<String>> {
    let Some(proposal) = db.worker_proposal(proposal_id)? else {
        return Ok(None);
    };
    if proposal.kind != boss_protocol::ProposalKind::ReviewVerdict
        || proposal.state != boss_protocol::ProposalState::Proposed
    {
        return Ok(None);
    }
    let inputs = db.review_verdict_comparison(proposal_id)?;
    let should_observe = if inputs.comparison.is_some() && inputs.pre_merge && !inputs.has_materialization {
        match pr_checker
            .inspect(&inputs.pr_url)
            .ok()
            .and_then(|pr| pr.head_sha)
            .filter(|sha| !sha.is_empty())
        {
            Some(head) => head == inputs.target_sha,
            None => false,
        }
    } else {
        true
    };
    let deletions = if should_observe {
        Some(inputs.comparison.as_ref().map(observe).unwrap_or_default())
    } else {
        None
    };
    db.apply_observed_review_verdict_proposal(proposal_id, pr_checker, ReviewVerdictObservation { inputs, deletions })
}

pub(crate) fn observe_merge_parent_deletions(input: &MergeParentComparison) -> Vec<String> {
    crate::merge_parent_deletion::compute_merged_parent_deletions_blocking(
        &input.repo_slug,
        &input.head_before,
        &input.base_sha,
        &input.head_after,
    )
}

/// Apply every still-`proposed` `review_verdict`. Safe to call
/// redundantly: a proposal already applied is skipped, and the
/// `pr_review:` created_via key plus the batch/proposal unique indexes
/// make rematerialisation a no-op.
pub(crate) fn apply_pending_review_verdicts(
    db: &WorkDb,
    pr_checker: &dyn PrStateChecker,
    observe: &dyn Fn(&MergeParentComparison) -> Vec<String>,
) -> Result<ReviewVerdictApplyStats> {
    let pending = db.list_proposed_review_verdicts()?;
    let mut stats = ReviewVerdictApplyStats::default();
    for proposal in pending {
        match apply_review_verdict_proposal(db, &proposal.id, pr_checker, observe) {
            Ok(created) => {
                // `apply_review_verdict_proposal` also returns `Ok(None)`
                // for a proposal it deliberately left `proposed` for
                // retry (e.g. the batch hasn't reached `applying` yet)
                // and for a proposal that landed `superseded` in the
                // materialise-to-commit window. Re-check the row's own
                // state rather than trusting the call's `Ok`/`Err`
                // split, so `applied` means `ProposalState::Applied`.
                match db.worker_proposal(&proposal.id) {
                    Ok(Some(WorkerProposal {
                        state: ProposalState::Applied,
                        ..
                    })) => {
                        stats.applied += 1;
                        if created.is_some() {
                            stats.created_work += 1;
                        }
                    }
                    Ok(Some(WorkerProposal {
                        state: ProposalState::Superseded,
                        ..
                    })) => {
                        stats.superseded += 1;
                    }
                    _ => {}
                }
            }
            Err(error) => {
                stats.failed += 1;
                tracing::warn!(
                    proposal_id = %proposal.id,
                    ?error,
                    "review-verdict apply failed; leaving the proposal proposed for retry",
                );
            }
        }
    }
    Ok(stats)
}

// Test fixtures exercise the same service with their hermetic compare responses.
#[cfg(test)]
impl WorkDb {
    pub(crate) fn apply_review_verdict_proposal(
        &self,
        proposal_id: &str,
        checker: &dyn PrStateChecker,
    ) -> anyhow::Result<Option<String>> {
        apply_review_verdict_proposal(self, proposal_id, checker, &|input| {
            checker.merged_parent_deletions(&input.repo_slug, &input.head_before, &input.base_sha, &input.head_after)
        })
    }
    pub(crate) fn apply_pending_review_verdicts(
        &self,
        checker: &dyn PrStateChecker,
    ) -> anyhow::Result<ReviewVerdictApplyStats> {
        apply_pending_review_verdicts(self, checker, &|input| {
            checker.merged_parent_deletions(&input.repo_slug, &input.head_before, &input.base_sha, &input.head_after)
        })
    }
}

/// Cadence for the crash-recovery pass. Fresh submissions are applied
/// immediately from the proposal handler; this interval only covers
/// engine restarts and a failed first attempt.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

impl crate::sweep_loop::SweepOutcome for ReviewVerdictApplyStats {
    fn has_activity(&self) -> bool {
        self.applied > 0 || self.failed > 0 || self.superseded > 0
    }

    fn log(&self) {
        tracing::info!(
            applied = self.applied,
            failed = self.failed,
            created_work = self.created_work,
            superseded = self.superseded,
            "review-verdict apply sweep: applied staged review_verdict proposals",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_work_sweep_loop(
        work_db,
        coordinator,
        dispatch_events,
        interval,
        |work_db, coordinator, _dispatch_events| Box::pin(run_one_pass(work_db, coordinator)),
    )
}

/// Apply every still-proposed `review_verdict`. Kicks the scheduler when
/// a revision or follow-up was created so it does not wait for the next
/// heartbeat.
pub async fn run_one_pass(work_db: &WorkDb, coordinator: Arc<ExecutionCoordinator>) -> ReviewVerdictApplyStats {
    let work_db_clone = work_db.clone();
    let stats = match tokio::task::spawn_blocking(move || {
        apply_pending_review_verdicts(&work_db_clone, &GhPrStateChecker, &observe_merge_parent_deletions)
    })
    .await
    {
        Ok(Ok(stats)) => stats,
        Ok(Err(error)) => {
            tracing::warn!(
                ?error,
                "review-verdict apply sweep: apply query failed; skipping this pass"
            );
            ReviewVerdictApplyStats::default()
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                "review-verdict apply sweep: apply task joined with error; skipping this pass"
            );
            ReviewVerdictApplyStats::default()
        }
    };
    if stats.created_work > 0 {
        coordinator.kick();
    }
    stats
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use crate::test_support::{create_active_chore, create_product, open_db};
    use crate::work::{
        ConflictResolutionInsertInput, FakePrStateChecker, PrOpenState, ReviewBatchCreateInput,
        ReviewBatchMemberCreateInput, SubmitWorkerProposalInput,
    };
    use boss_protocol::{
        CreateExecutionInput, ExecutionKind, ExecutionStatus, ProposalKind, ReviewBatchMemberRole,
        ReviewBatchMemberStatus, ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket, ReviewProfile,
    };

    #[test]
    fn a_comparison_changed_during_observation_is_retried_and_the_owned_deletions_hold_the_root() {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let root = create_active_chore(&db, &product, "merge resolution review");
        let pr_url = "https://github.com/acme/widget/pull/9";
        db.connect().unwrap().execute(
            "UPDATE tasks SET pr_url = ?2, status = 'in_review', repo_remote_url = 'https://github.com/acme/widget' WHERE id = ?1",
            rusqlite::params![root, pr_url],
        ).unwrap();
        let conflict = db
            .insert_conflict_resolution(ConflictResolutionInsertInput {
                product_id: product,
                work_item_id: root.clone(),
                pr_url: pr_url.to_owned(),
                pr_number: 9,
                head_branch: "feature".to_owned(),
                base_branch: "main".to_owned(),
                base_sha_at_trigger: Some("base".to_owned()),
                head_sha_before: Some("before".to_owned()),
            })
            .unwrap()
            .unwrap();
        db.mark_conflict_resolution_succeeded(&conflict.id, Some("head"))
            .unwrap();
        let supervisor = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(root.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let classification = ReviewClassification::builder()
            .changed_files(vec!["src/lib.rs".to_owned()])
            .complexity_flags(vec![])
            .has_production_code(true)
            .metadata_missing(vec![])
            .production_languages(vec![ReviewLanguageBucket::Rust])
            .profile(ReviewProfile::Light)
            .subsystem_buckets(vec!["engine".to_owned()])
            .additions(1)
            .deletions(1)
            .build();
        let (batch, _) = db
            .create_review_batch(
                ReviewBatchCreateInput::builder()
                    .cycle_root_id(root.clone())
                    .base_sha("base")
                    .classification(classification)
                    .phase(ReviewBatchPhase::PreMerge)
                    .pr_number(9)
                    .pr_url(pr_url)
                    .target_sha("head")
                    .build(),
                &[ReviewBatchMemberCreateInput::builder()
                    .attempt(1)
                    .provider_effort("medium")
                    .requested_driver("claude")
                    .resolved_model("test-model")
                    .role(ReviewBatchMemberRole::Supervisor)
                    .status(ReviewBatchMemberStatus::Pending)
                    .execution_id(supervisor.id.clone())
                    .build()],
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE pr_review_batches SET status = 'supervising' WHERE id = ?1",
                [&batch.id],
            )
            .unwrap();
        let payload = serde_json::json!({"batch_id": batch.id, "verdict": {
            "batch_id": batch.id, "pr_url": pr_url, "target_sha": "head", "phase": "pre_merge",
            "summary": "Clean review", "revision_warranted": false, "findings": [], "contradictions": []
        }})
        .to_string();
        let proposal = db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &supervisor.id,
                work_item_id: &root,
                kind: ProposalKind::ReviewVerdict,
                payload_json: &payload,
                idempotency_key: "verdict",
            })
            .unwrap()
            .unwrap()
            .proposal;
        let checker = FakePrStateChecker::always(PrOpenState::Open).with_head_sha("head");

        let error = apply_review_verdict_proposal(&db, &proposal.id, &checker, &|input| {
            assert_eq!(input.repo_slug, "acme/widget");
            assert_eq!(
                (&*input.head_before, &*input.base_sha, &*input.head_after),
                ("before", "base", "head")
            );
            // Re-entering storage here also proves the service holds no connection lock.
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE tasks SET repo_remote_url = 'https://github.com/acme/renamed' WHERE id = ?1",
                    [&root],
                )
                .unwrap();
            vec!["src/lib.rs".to_owned()]
        })
        .unwrap_err();
        assert!(error.to_string().contains("comparison changed during observation"));
        assert_eq!(
            db.worker_proposal(&proposal.id).unwrap().unwrap().state,
            ProposalState::Proposed
        );
        assert!(
            matches!(db.get_work_item(&root).unwrap(), boss_protocol::WorkItem::Chore(task) if task.status == boss_protocol::TaskStatus::InReview)
        );

        apply_review_verdict_proposal(&db, &proposal.id, &checker, &|input| {
            assert_eq!(input.repo_slug, "acme/renamed");
            vec!["src/lib.rs".to_owned()]
        })
        .unwrap();
        assert!(
            matches!(db.get_work_item(&root).unwrap(), boss_protocol::WorkItem::Chore(task) if task.status == boss_protocol::TaskStatus::Blocked)
        );
        assert!(
            db.list_attention_items_for_work_item(&root)
                .unwrap()
                .iter()
                .any(|item| item.kind == crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND && item.status == "open")
        );
        assert_eq!(
            db.worker_proposal(&proposal.id).unwrap().unwrap().state,
            ProposalState::Applied
        );
    }
}

pub(crate) mod guide_validation {
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
}

/// Post-commit review-guide notifications and their publication contract.
pub(crate) mod publication {
    use crate::coordinator::ExecutionPublisher;
    use crate::work::WorkDb;
    use std::sync::Arc;

    /// Broadcast a work-item invalidation for `root_task_id` after a review-guide
    /// lifecycle transition that did not itself touch the `tasks` row (guide
    /// state lives in `pr_review_guide_source_series` / `_attempts` /
    /// `_versions`, keyed by `root_task_id`, not in `tasks`). Mirrors the
    /// `"task_doc_pointer_set"` / `"design_doc_pointer_set"` precedent in
    /// `completion/pr_transition.rs`: a derived projection changed on read, so
    /// subscribers (the kanban view) need a refetch hint, not a payload push.
    /// Best-effort — a failure to resolve the task's product only means the
    /// card catches up on its next unrelated refresh instead of immediately.
    /// Takes the publisher directly (rather than `Arc<ServerState>`) so both a
    /// `Dispatch` request handler (`server_state.publisher`) and
    /// `WorkerCompletionHandler` (its own `publisher` field) can share it.
    pub(crate) async fn notify_review_guide_changed(
        work_db: &WorkDb,
        publisher: &Arc<dyn ExecutionPublisher>,
        root_task_id: &str,
        reason: &str,
    ) {
        match work_db.get_work_item(root_task_id) {
            Ok(item) => {
                publisher
                    .publish_work_item_changed(item.product_id(), root_task_id, reason)
                    .await;
            }
            Err(err) => {
                tracing::warn!(
                    root_task_id,
                    reason,
                    ?err,
                    "notify_review_guide_changed: failed to resolve root task's product; skipping broadcast"
                );
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        use crate::test_support::{
            RecordingPublisher, create_active_chore, create_product, open_db, seed_review_guide_series,
        };
        use crate::work::PublishReviewGuideOutcome;

        /// Records the guide's readable pointer at the instant the publisher is
        /// called, so a test can prove the announcement follows the commit.
        struct ReadablePointerProbe {
            db: WorkDb,
            root_task_id: String,
            readable_at_publish: std::sync::Mutex<Vec<Option<String>>>,
        }

        #[async_trait::async_trait]
        impl ExecutionPublisher for ReadablePointerProbe {
            async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}

            async fn publish_work_item_changed(&self, _: &str, _: &str, _: &str) {
                let summary = self
                    .db
                    .get_pr_review_guide_summary_for_root(&self.root_task_id)
                    .unwrap();
                self.readable_at_publish
                    .lock()
                    .unwrap()
                    .push(summary.and_then(|summary| summary.readable_version_id));
            }

            async fn publish_frontend_event_on_product(&self, _: &str, _: boss_protocol::FrontendEvent) {}
        }

        #[tokio::test]
        async fn publishes_the_root_cards_product_invalidation_with_the_given_reason() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            let root = create_active_chore(&db, &product, "guide publication");
            let recorder = Arc::new(RecordingPublisher::default());
            let publisher: Arc<dyn ExecutionPublisher> = recorder.clone();

            notify_review_guide_changed(&db, &publisher, &root, "review_guide_ready").await;

            assert_eq!(
                *recorder.events.lock().await,
                vec![(product, root, "review_guide_ready".to_owned())]
            );
        }

        #[tokio::test]
        async fn an_unresolvable_root_task_is_skipped_without_publishing() {
            let (_dir, db) = open_db();
            let recorder = Arc::new(RecordingPublisher::default());
            let publisher: Arc<dyn ExecutionPublisher> = recorder.clone();

            notify_review_guide_changed(&db, &publisher, "task_does_not_exist", "review_guide_failed").await;

            assert!(recorder.events.lock().await.is_empty());
        }

        /// The announcement is made by the caller after storage commits, never
        /// from inside the transaction: when the publisher runs, a subscriber that
        /// re-reads the card already sees the published guide.
        #[tokio::test]
        async fn the_announcement_follows_the_commit_that_published_the_guide() {
            let (_dir, db) = open_db();
            let product = create_product(&db);
            let root = create_active_chore(&db, &product, "guide publication");
            let (series_id, comparison_id) = seed_review_guide_series(&db, &root);
            let attempt = db
                .create_pr_review_guide_attempt(&series_id, &comparison_id, boss_review_guide::PROMPT_VERSION)
                .unwrap();
            let execution = db
                .create_pr_review_guide_execution(&comparison_id, "acme/widget")
                .unwrap();
            db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
                .unwrap();
            let probe = Arc::new(ReadablePointerProbe {
                db: db.clone(),
                root_task_id: root.clone(),
                readable_at_publish: Default::default(),
            });
            let publisher: Arc<dyn ExecutionPublisher> = probe.clone();

            let PublishReviewGuideOutcome::Published(version) = db
                .publish_pr_review_guide_version(&attempt.id, "# Guide\n", "raw")
                .unwrap()
            else {
                panic!("the guide must publish");
            };
            notify_review_guide_changed(&db, &publisher, &root, "review_guide_ready").await;

            assert_eq!(
                *probe.readable_at_publish.lock().unwrap(),
                vec![Some(version.id)],
                "a subscriber re-reading on the announcement must see the committed guide"
            );
        }
    }
}
