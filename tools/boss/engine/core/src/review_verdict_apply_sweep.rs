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
    GhPrStateChecker, MergeParentComparison, PrStateChecker, PreMergeTargetFreshness, ReviewVerdictApplyStats,
    ReviewVerdictObservation, WorkDb,
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
    // The readiness check nudges quorum and files attention for stranded batches once.
    // Leave unready proposals for a later sweep without nudging them a second time.
    if !db.review_verdict_batch_applying(proposal_id)? {
        return Ok(None);
    }
    let inputs = db.review_verdict_comparison(proposal_id)?;
    // Freshness is decided once here and carried to storage, so `gh pr view` runs once per apply.
    let freshness = if inputs.pre_merge {
        Some(db.review_verdict_target_freshness(proposal_id, pr_checker)?)
    } else {
        None
    };
    let should_observe = match &freshness {
        Some(PreMergeTargetFreshness::Unknown) => false,
        Some(PreMergeTargetFreshness::SupersededBy(_)) => inputs.has_materialization,
        Some(PreMergeTargetFreshness::Current) | None => true,
    };
    let deletions = if should_observe {
        Some(inputs.comparison.as_ref().map(observe).unwrap_or_default())
    } else {
        None
    };
    db.apply_observed_review_verdict_proposal(
        proposal_id,
        pr_checker,
        ReviewVerdictObservation {
            inputs,
            freshness,
            deletions,
        },
    )
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

    /// A conflict-resolution PR with a staged clean verdict whose batch is `supervising` and not yet advanced.
    fn stage_conflict_resolution_verdict(db: &WorkDb) -> (String, WorkerProposal) {
        let product = create_product(db);
        let root = create_active_chore(db, &product, "merge resolution review");
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
        (root, proposal)
    }

    /// Counts the GitHub head lookups a sweep pass makes.
    struct CountingChecker {
        inner: FakePrStateChecker,
        inspects: std::sync::atomic::AtomicUsize,
    }

    impl crate::work::PrStateChecker for CountingChecker {
        fn check(&self, pr_url: &str) -> anyhow::Result<PrOpenState> {
            self.inner.check(pr_url)
        }

        fn inspect(&self, pr_url: &str) -> anyhow::Result<crate::work::PrInspect> {
            self.inspects.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.inspect(pr_url)
        }
    }

    #[test]
    fn a_stranded_batch_spends_no_github_calls_on_a_sweep_pass() {
        let (_dir, db) = open_db();
        let (root, proposal) = stage_conflict_resolution_verdict(&db);
        // The batch regresses after the verdict was staged, so the quorum nudge cannot advance it.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE pr_review_batches SET status = 'failed' WHERE cycle_root_id = ?1",
                [&root],
            )
            .unwrap();
        let checker = CountingChecker {
            inner: FakePrStateChecker::always(PrOpenState::Open).with_head_sha("head"),
            inspects: Default::default(),
        };
        let observed = std::sync::atomic::AtomicUsize::new(0);

        let created = apply_review_verdict_proposal(&db, &proposal.id, &checker, &|_| {
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            vec![]
        })
        .unwrap();

        assert_eq!(created, None);
        assert_eq!(checker.inspects.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            db.worker_proposal(&proposal.id).unwrap().unwrap().state,
            ProposalState::Proposed
        );
        assert!(
            db.list_attention_items_for_work_item(&root)
                .unwrap()
                .iter()
                .any(|item| item.kind == crate::work::REVIEW_VERDICT_BATCH_NOT_APPLYING_ATTENTION_KIND)
        );
    }

    #[test]
    fn an_applying_verdict_inspects_the_live_head_once() {
        let (_dir, db) = open_db();
        let (_root, proposal) = stage_conflict_resolution_verdict(&db);
        let checker = CountingChecker {
            inner: FakePrStateChecker::always(PrOpenState::Open).with_head_sha("head"),
            inspects: Default::default(),
        };

        apply_review_verdict_proposal(&db, &proposal.id, &checker, &|_| vec![]).unwrap();

        assert_eq!(checker.inspects.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            db.worker_proposal(&proposal.id).unwrap().unwrap().state,
            ProposalState::Applied
        );
    }

    #[test]
    fn a_comparison_changed_during_observation_is_retried_and_the_owned_deletions_hold_the_root() {
        let (_dir, db) = open_db();
        let (root, proposal) = stage_conflict_resolution_verdict(&db);
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
