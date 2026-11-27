//! Routing, quorum, guard, cycle-counting, and post-merge eligibility tests
//! for `delta_rereview` (one-member Light-tier delta re-review from round 2).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use tempfile::tempdir;

use super::*;
use crate::ReviewBatchEnqueuer;
use crate::feature_flags::{
    DELTA_REREVIEW_AB_SPLIT_FLAG, DELTA_REREVIEW_EXPERIMENT, FeatureFlagsStore, TOKEN_EXPERIMENTS_FLAG,
};
use crate::review_admission::{ReviewAdmission, admit_review_with_mode};
use crate::run_done_declaration::maybe_enqueue_declared_delivery_reviewer;
use crate::should_use_delta_rereview;
use crate::work::{
    PreMergeTargetFreshness, REVIEW_GATE_OUTCOME_SKIPPED_CYCLE_CAP, RetryDeadReviewBatchMember, ReviewBatchCreateInput,
    ReviewBatchDispatch, ReviewBatchQuorumOutcome, ReviewVerdictObservation, SubmitWorkerProposalInput, WorkItemPatch,
    experiment_bucket,
};
use boss_observation_contracts::OpenPrMergeability;
use boss_protocol::{
    CreateChoreInput, CreateRevisionInput, ExecutionKind, ExecutionStatus, ProposalKind, ProposalState,
    ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase, ReviewBatchStatus, ReviewClassification,
    ReviewLanguageBucket, ReviewProfile, ReviewReportProposalPayload, ReviewVerdictProposalPayload,
};

const REPO: &str = "git@github.com:spinyfin/mono.git";

fn test_classification(profile: ReviewProfile) -> ReviewClassification {
    ReviewClassification::builder()
        .changed_files(vec!["tools/boss/engine/completion/src/pr_transition.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(profile)
        .subsystem_buckets(vec!["tools/boss/engine".to_owned()])
        .additions(24)
        .deletions(6)
        .build()
}

/// Test enqueuer that mirrors `GhReviewBatchEnqueuer`'s production contract
/// without calling `gh`: `enqueue_with_mode` forwards the caller's flag, and
/// plain `enqueue` is always the full multi-leaf batch. Routing lives in the
/// trigger sites, never in the enqueuer.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct ModeAwareStubEnqueuer {
    head_sha: String,
    #[builder(default = 24)]
    additions: i64,
    #[builder(default = 6)]
    deletions: i64,
    #[builder(default)]
    calls: AtomicUsize,
    #[builder(default)]
    last_single_reviewer: AtomicBool,
}

impl ModeAwareStubEnqueuer {
    fn new(head_sha: &str) -> Arc<Self> {
        Arc::new(Self {
            head_sha: head_sha.to_owned(),
            additions: 24,
            deletions: 6,
            calls: AtomicUsize::new(0),
            last_single_reviewer: AtomicBool::new(false),
        })
    }

    fn with_diff_size(head_sha: &str, additions: i64, deletions: i64) -> Arc<Self> {
        Arc::new(Self {
            head_sha: head_sha.to_owned(),
            additions,
            deletions,
            calls: AtomicUsize::new(0),
            last_single_reviewer: AtomicBool::new(false),
        })
    }

    fn metadata_json(&self) -> serde_json::Value {
        serde_json::json!({
            "state": "OPEN",
            "baseRefOid": "base-sha-000",
            "headRefOid": self.head_sha,
            "files": [{"path": "tools/boss/engine/completion/src/pr_transition.rs"}],
            "additions": self.additions,
            "deletions": self.deletions,
        })
    }
}

#[async_trait]
impl ReviewBatchEnqueuer for ModeAwareStubEnqueuer {
    async fn enqueue(
        &self,
        work_db: &WorkDb,
        work_item_id: &str,
        repo_remote_url: &str,
        pr_url: &str,
        review_pool_size: usize,
    ) -> anyhow::Result<ReviewBatchDispatch> {
        self.enqueue_with_mode(work_db, work_item_id, repo_remote_url, pr_url, review_pool_size, false)
            .await
    }

    async fn enqueue_with_mode(
        &self,
        work_db: &WorkDb,
        work_item_id: &str,
        repo_remote_url: &str,
        pr_url: &str,
        review_pool_size: usize,
        single_reviewer_light: bool,
    ) -> anyhow::Result<ReviewBatchDispatch> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.last_single_reviewer.store(single_reviewer_light, Ordering::SeqCst);
        let mut input = crate::review_batch_input_from_metadata(work_db, work_item_id, pr_url, self.metadata_json())?;
        input.single_reviewer_light = single_reviewer_light;
        work_db.create_pre_merge_review_batch_for_pool(input, repo_remote_url, review_pool_size)
    }
}

struct HeadProbe(String);

#[async_trait]
impl MergeProbe for HeadProbe {
    async fn probe(&self, url: &str) -> anyhow::Result<PrLifecycleProbe> {
        let mut probe = FixedMergeabilityProbe {
            mergeability: OpenPrMergeability::Clean,
        }
        .probe(url)
        .await?;
        probe.head_ref_oid = Some(self.0.clone());
        Ok(probe)
    }
}

fn delta_flags(dir: &std::path::Path) -> Arc<FeatureFlagsStore> {
    let flags = Arc::new(FeatureFlagsStore::new(dir.join("feature-flags.toml")));
    flags.load().unwrap();
    flags.set("review_batch_fanout", true).unwrap();
    flags.set(TOKEN_EXPERIMENTS_FLAG, true).unwrap();
    flags.set(DELTA_REREVIEW_EXPERIMENT, true).unwrap();
    flags.set(DELTA_REREVIEW_AB_SPLIT_FLAG, true).unwrap();
    flags
}

/// Create a root chore whose deterministic `delta_rereview` bucket falls in the
/// requested arm (`treated == true` -> bucket < 50; `treated == false` -> bucket >= 50).
fn create_chore_in_arm(db: &WorkDb, product_id: &str, pr_url: &str, treated: bool) -> String {
    for idx in 0..200 {
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product_id.to_owned())
                    .name(format!("delta-rereview-{treated}-{idx}"))
                    .description("Implement feature")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let in_treatment = experiment_bucket(&chore.id, DELTA_REREVIEW_EXPERIMENT) < 50;
        if in_treatment == treated {
            db.update_work_item(
                &chore.id,
                WorkItemPatch {
                    pr_url: Some(pr_url.to_owned()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
            return chore.id;
        }
    }
    panic!("failed to draw chore in arm treated={treated}");
}

fn start_running_execution(
    db: &WorkDb,
    work_item_id: &str,
    kind: ExecutionKind,
    workspace: &std::path::Path,
) -> String {
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id)
                .kind(kind)
                .status(ExecutionStatus::Ready)
                .repo_remote_url(REPO)
                .build(),
        )
        .unwrap();
    let (_, run) = db
        .start_execution_run(
            &exec.id,
            "worker-1",
            "mono",
            "lease-1",
            "ws-1",
            workspace.to_str().unwrap(),
        )
        .unwrap();
    finish_run_worker_pane_alive(db, &exec.id, &run.id, None);
    exec.id
}

fn submit_leaf_report(
    db: &WorkDb,
    execution_id: &str,
    work_item_id: &str,
    batch_id: &str,
    pr_url: &str,
    target_sha: &str,
) {
    let payload = ReviewReportProposalPayload {
        batch_id: batch_id.to_owned(),
        target_sha: target_sha.to_owned(),
        report: serde_json::json!({
            "batch_id": batch_id,
            "pr_url": pr_url,
            "target_sha": target_sha,
            "phase": "pre_merge",
            "summary": "Found an edge-case issue.",
            "coverage": {
                "files_inspected": ["src/handler.rs"],
                "files_omitted": [],
                "limitations": [],
            },
            "findings": [{
                "severity": "high",
                "category": "correctness",
                "confidence": "high",
                "file": "src/handler.rs",
                "location": "fn handle, ~L42",
                "title": "Missing empty-input guard",
                "problem": "handle() panics when input slice is empty.",
                "impact": "Panics on empty payloads.",
                "suggested_fix": "Guard empty slice before indexing.",
                "static_evidence": "Direct index bytes[0] with no len check.",
                "needs_runtime_verification": false,
            }]
        }),
    };
    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id,
            work_item_id,
            kind: ProposalKind::ReviewReport,
            payload_json: &serde_json::to_string(&payload).unwrap(),
            idempotency_key: &format!("report-{execution_id}"),
        })
        .unwrap()
        .unwrap();
    assert_eq!(outcome.proposal.state, ProposalState::Applied);
}

fn build_verdict_json(
    batch_id: &str,
    pr_url: &str,
    target_sha: &str,
    summary: &str,
    revision_warranted: bool,
    findings: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "batch_id": batch_id,
        "pr_url": pr_url,
        "target_sha": target_sha,
        "phase": "pre_merge",
        "summary": summary,
        "revision_warranted": revision_warranted,
        "findings": findings,
        "rejected": [],
        "contradictions": [],
    })
}

fn apply_verdict(db: &WorkDb, proposal_id: &str, freshness: PreMergeTargetFreshness) -> Option<String> {
    let inputs = db.review_verdict_comparison(proposal_id).unwrap();
    let deletions = (!matches!(freshness, PreMergeTargetFreshness::SupersededBy(_))).then(Vec::new);
    db.apply_observed_review_verdict_proposal(
        proposal_id,
        &FakePrStateChecker::always(PrOpenState::Open),
        ReviewVerdictObservation {
            inputs,
            freshness: Some(freshness),
            deletions,
        },
    )
    .unwrap()
}

/// Every automatic pre-merge trigger site (`finalize_pr_transition`,
/// `maybe_enqueue_declared_delivery_reviewer`, `admit_review_with_mode` +
/// `should_use_delta_rereview` for unpark reconcile, and deferred admission
/// through `ReviewBatchEnqueuer::enqueue`) must agree on:
/// - Round 1 (`review_cycle == 0`): full two-leaf batch even for treated families.
/// - Round 2+ (`review_cycle >= 1`, `last_reviewed_sha` set):
///   - Treated family -> 1-member Light-tier (`sonnet`, `medium`) batch.
///   - Control family (split on) -> full two-leaf batch.
///   - Split off -> control-bucket family also gets 1-member Light-tier batch.
///   - `delta_rereview` or `token_experiments` off -> full two-leaf batch.
#[tokio::test]
async fn all_trigger_sites_agree_on_round_1_full_batch_and_round_2_delta_routing() {
    let (_db_dir, db) = open_db_arc();
    let workspace = tempdir().unwrap();
    let flags_dir = tempdir().unwrap();
    let flags = delta_flags(flags_dir.path());
    let product = create_test_product(&db);

    let treated_pr = "https://github.com/spinyfin/mono/pull/4101";
    let control_pr = "https://github.com/spinyfin/mono/pull/4102";
    let treated_root = create_chore_in_arm(&db, &product.id, treated_pr, true);
    let control_root = create_chore_in_arm(&db, &product.id, control_pr, false);

    // 1. Round 1 (review_cycle == 0): treated family MUST still take the full batch.
    assert!(
        !should_use_delta_rereview(&flags, &db, &treated_root),
        "round 1 must stay on the full batch even for a treated family"
    );
    let r1_exec = start_running_execution(&db, &treated_root, ExecutionKind::ChoreImplementation, workspace.path());
    let r1_enqueuer = ModeAwareStubEnqueuer::new("sha-r1");
    let verifier = StubBranchVerifier::ok("boss/test");
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_enable_revision_triggered_reviews(true)
        .with_feature_flags(flags.clone())
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(HeadProbe("sha-r1".to_owned())))
        .with_review_batch_enqueuer(r1_enqueuer.clone());
    let outcome = handler
        .finalize_pr_transition(
            &r1_exec,
            treated_pr.to_owned(),
            WorkerPrCompletionTarget::InReview,
            "test-r1",
        )
        .await;
    assert_eq!(
        outcome,
        StopOutcome::ReviewerEnqueued {
            pr_url: treated_pr.to_owned()
        }
    );
    assert_eq!(r1_enqueuer.calls.load(Ordering::SeqCst), 1);
    assert!(
        !r1_enqueuer.last_single_reviewer.load(Ordering::SeqCst),
        "finalize_pr_transition must not use delta re-review on round 1"
    );
    let r1_batches = db.review_batches_for_cycle_root(&treated_root).unwrap();
    assert_eq!(r1_batches.len(), 1);
    assert!(!r1_batches[0].single_reviewer);
    assert_eq!(db.review_batch_members(&r1_batches[0].id).unwrap().len(), 2);

    // Mark round 1 completed on both roots (`review_cycle = 1`, `last_reviewed_sha = sha-r1`).
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'completed' WHERE id = ?1",
            [&r1_batches[0].id],
        )
        .unwrap();
    for root_id in [&treated_root, &control_root] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'in_review', review_cycle = 1, last_reviewed_sha = 'sha-r1', pr_head_sha = 'sha-r1' WHERE id = ?1",
                [root_id],
            )
            .unwrap();
    }

    // 2. Predicate matrix on round 2 across flag combinations.
    assert!(should_use_delta_rereview(&flags, &db, &treated_root));
    assert!(!should_use_delta_rereview(&flags, &db, &control_root));

    // With A/B split disabled, even a control-bucket family gets delta re-review.
    flags.set(DELTA_REREVIEW_AB_SPLIT_FLAG, false).unwrap();
    assert!(should_use_delta_rereview(&flags, &db, &control_root));
    flags.set(DELTA_REREVIEW_AB_SPLIT_FLAG, true).unwrap();

    // With `delta_rereview` off, treated family falls back to full batch.
    flags.set(DELTA_REREVIEW_EXPERIMENT, false).unwrap();
    assert!(!should_use_delta_rereview(&flags, &db, &treated_root));
    flags.set(DELTA_REREVIEW_EXPERIMENT, true).unwrap();

    // With `token_experiments` master switch off, treated family falls back to full batch.
    flags.set(TOKEN_EXPERIMENTS_FLAG, false).unwrap();
    assert!(!should_use_delta_rereview(&flags, &db, &treated_root));
    flags.set(TOKEN_EXPERIMENTS_FLAG, true).unwrap();

    // With required `review_batch_fanout` off, `delta_rereview` is inactive.
    flags.set("review_batch_fanout", false).unwrap();
    assert!(!should_use_delta_rereview(&flags, &db, &treated_root));
    flags.set("review_batch_fanout", true).unwrap();

    // 3. Trigger site A: `finalize_pr_transition` on a revision of the treated root.
    let rev1 = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(treated_root.clone())
                .description("Fix round 1 findings")
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    let rev1_exec = start_running_execution(&db, &rev1.id, ExecutionKind::RevisionImplementation, workspace.path());
    let r2_enqueuer = ModeAwareStubEnqueuer::new("sha-r2-finalize");
    let verifier = StubBranchVerifier::ok("boss/test");
    *verifier.fresh_head_oid_result.try_lock().unwrap() = Ok("sha-r2-finalize".to_owned());
    *verifier.head_oid_result.try_lock().unwrap() = Ok("sha-r2-finalize".to_owned());
    let handler_r2 = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_enable_revision_triggered_reviews(true)
        .with_feature_flags(flags.clone())
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(HeadProbe("sha-r2-finalize".to_owned())))
        .with_review_batch_enqueuer(r2_enqueuer.clone());
    let outcome = handler_r2
        .finalize_pr_transition(
            &rev1_exec,
            treated_pr.to_owned(),
            WorkerPrCompletionTarget::InReview,
            "test-r2",
        )
        .await;
    assert_eq!(
        outcome,
        StopOutcome::ReviewerEnqueued {
            pr_url: treated_pr.to_owned()
        }
    );
    assert!(r2_enqueuer.last_single_reviewer.load(Ordering::SeqCst));
    let batches = db.review_batches_for_cycle_root(&treated_root).unwrap();
    let r2_batch = batches
        .iter()
        .find(|b| b.target_sha == "sha-r2-finalize")
        .expect("round 2 delta batch created");
    assert!(r2_batch.single_reviewer);
    let r2_members = db.review_batch_members(&r2_batch.id).unwrap();
    assert_eq!(r2_members.len(), 1, "delta round must create exactly one member");
    assert_eq!(r2_members[0].role, ReviewBatchMemberRole::ClaudeReviewer);
    assert_eq!(r2_members[0].requested_driver, "claude");
    assert_eq!(r2_members[0].resolved_model, "sonnet");
    assert_eq!(r2_members[0].provider_effort, "medium");

    // Control family on round 2 via `finalize_pr_transition` stays full 2-leaf batch.
    let ctrl_rev = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(control_root.clone())
                .description("Fix control round 1 findings")
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    let ctrl_exec = start_running_execution(
        &db,
        &ctrl_rev.id,
        ExecutionKind::RevisionImplementation,
        workspace.path(),
    );
    let ctrl_enqueuer = ModeAwareStubEnqueuer::new("sha-ctrl-r2");
    let verifier = StubBranchVerifier::ok("boss/test");
    *verifier.fresh_head_oid_result.try_lock().unwrap() = Ok("sha-ctrl-r2".to_owned());
    *verifier.head_oid_result.try_lock().unwrap() = Ok("sha-ctrl-r2".to_owned());
    let handler_ctrl = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_enable_revision_triggered_reviews(true)
        .with_feature_flags(flags.clone())
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(HeadProbe("sha-ctrl-r2".to_owned())))
        .with_review_batch_enqueuer(ctrl_enqueuer.clone());
    let outcome = handler_ctrl
        .finalize_pr_transition(
            &ctrl_exec,
            control_pr.to_owned(),
            WorkerPrCompletionTarget::InReview,
            "test-ctrl-r2",
        )
        .await;
    assert_eq!(
        outcome,
        StopOutcome::ReviewerEnqueued {
            pr_url: control_pr.to_owned()
        }
    );
    assert!(!ctrl_enqueuer.last_single_reviewer.load(Ordering::SeqCst));
    let ctrl_batch = db
        .review_batches_for_cycle_root(&control_root)
        .unwrap()
        .into_iter()
        .find(|b| b.target_sha == "sha-ctrl-r2")
        .expect("control round 2 batch created");
    assert!(!ctrl_batch.single_reviewer);
    assert_eq!(db.review_batch_members(&ctrl_batch.id).unwrap().len(), 2);

    // 4. Trigger site B: `maybe_enqueue_declared_delivery_reviewer` (run_done path).
    let rev2 = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(treated_root.clone())
                .description("Second revision via run_done")
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    let rev2_exec_id = start_running_execution(&db, &rev2.id, ExecutionKind::RevisionImplementation, workspace.path());
    let rev2_exec = db.get_execution(&rev2_exec_id).unwrap();
    let run_done_enqueuer = ModeAwareStubEnqueuer::new("sha-r2-rundone");
    let run_done_verifier = StubBranchVerifier::ok("boss/test");
    *run_done_verifier.fresh_head_oid_result.try_lock().unwrap() = Ok("sha-r2-rundone".to_owned());
    *run_done_verifier.head_oid_result.try_lock().unwrap() = Ok("sha-r2-rundone".to_owned());
    let harness = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    maybe_enqueue_declared_delivery_reviewer(
        &db,
        harness.publisher.as_ref(),
        run_done_verifier.as_ref(),
        run_done_enqueuer.as_ref(),
        &flags,
        5,
        0,
        16,
        &rev2_exec,
        &rev2.id,
        REPO,
        treated_pr,
    )
    .await;
    assert!(run_done_enqueuer.last_single_reviewer.load(Ordering::SeqCst));
    let rundone_batch = db
        .review_batches_for_cycle_root(&treated_root)
        .unwrap()
        .into_iter()
        .find(|b| b.target_sha == "sha-r2-rundone")
        .expect("run_done delta batch created");
    assert!(rundone_batch.single_reviewer);
    assert_eq!(db.review_batch_members(&rundone_batch.id).unwrap().len(), 1);

    // 5. Trigger site C's admission call: `admit_review_with_mode` with the mode `CompletionRequests::should_use_delta_rereview` returns.
    let unpark_handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_feature_flags(flags.clone());
    assert!(unpark_handler.should_use_delta_rereview(&db, &treated_root));
    assert!(!unpark_handler.should_use_delta_rereview(&db, &control_root));
    let unpark_admission = admit_review_with_mode(
        &db,
        &treated_root,
        Some(treated_pr),
        Some(16),
        unpark_handler.should_use_delta_rereview(&db, &treated_root),
        &FakePrStateChecker::always(PrOpenState::Open),
        |_| async {
            Ok(serde_json::json!({
                "state": "OPEN",
                "baseRefOid": "base-sha-000",
                "headRefOid": "sha-r2-unpark",
                "files": [{"path": "src/lib.rs"}],
                "additions": 10,
                "deletions": 2,
            }))
        },
    )
    .await
    .unwrap();
    match unpark_admission {
        ReviewAdmission::Batch(ReviewBatchDispatch::Created { batch, executions }) => {
            assert!(batch.single_reviewer);
            assert_eq!(executions.len(), 1);
        }
        _ => panic!("expected solo batch from unpark admission"),
    }

    // Trigger site D (the merge poller's deferred-admission sweep) and the
    // production wiring of site C (unpark reconcile's `ai_review_step`) cannot
    // be built from this crate: they are exercised against the real sweep and
    // the real step, with the real routing decision, in the merge-poller's
    // `deferred_review_admission_tests` and core's `unpark_reconcile` steps tests.
}

/// Multi-round integration across both paths:
/// Round 1 (two-leaf + supervisor) -> `review_cycle = 1`, `last_reviewed_sha = sha-r1`,
/// structured findings stored -> `latest_prior_pre_merge_review_round` yields round 1 findings
/// -> Round 2 (one-member delta batch) rejects `review_report`, accepts `review_verdict`
/// directly `collecting -> applying` with ZERO supervisor -> `apply_observed_review_verdict_proposal`
/// increments `review_cycle = 2`, stamps `last_reviewed_sha = sha-r2`, mints revision, stores
/// round 2 findings -> Round 3 hits `max_review_cycles = 2` and skips with `skipped_cycle_cap`.
#[tokio::test]
async fn cycle_counting_and_prior_findings_carry_seamlessly_across_fanout_and_delta_paths() {
    let (_db_dir, db) = open_db_arc();
    let workspace = tempdir().unwrap();
    let flags_dir = tempdir().unwrap();
    let flags = delta_flags(flags_dir.path());
    let product = create_test_product(&db);
    let pr_url = "https://github.com/spinyfin/mono/pull/4201";
    let root_id = create_chore_in_arm(&db, &product.id, pr_url, true);

    db.update_work_item(
        &root_id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(pr_url.to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // Round 1: full two-leaf batch.
    let r1_input = ReviewBatchCreateInput::builder()
        .cycle_root_id(root_id.clone())
        .base_sha("base-0")
        .classification(test_classification(ReviewProfile::Standard))
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(4201)
        .pr_url(pr_url)
        .target_sha("sha-r1")
        .build();
    let (r1_batch, r1_execs) = match db.create_pre_merge_review_batch(r1_input, REPO).unwrap() {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected round 1 batch, got {other:?}"),
    };
    assert!(!r1_batch.single_reviewer);
    assert_eq!(r1_execs.len(), 2);

    // Both leaves submit review_report -> quorum dispatches supervisor.
    for exec in &r1_execs {
        submit_leaf_report(&db, &exec.id, &root_id, &r1_batch.id, pr_url, "sha-r1");
    }
    let r1_members = db.review_batch_members(&r1_batch.id).unwrap();
    let sup_member = r1_members
        .iter()
        .find(|m| m.role == ReviewBatchMemberRole::Supervisor)
        .expect("round 1 supervisor dispatched");
    let sup_exec_id = sup_member.execution_id.as_deref().unwrap();

    // Supervisor submits verdict with 1 high finding citing both leaves' engine-assigned ids.
    let r1_verdict_payload = ReviewVerdictProposalPayload {
        batch_id: r1_batch.id.clone(),
        verdict: build_verdict_json(
            &r1_batch.id,
            pr_url,
            "sha-r1",
            "Round 1 found missing guard.",
            true,
            serde_json::json!([{
                "id": "C1",
                "severity": "high",
                "category": "correctness",
                "confidence": "high",
                "file": "src/handler.rs",
                "location": "fn handle, ~L42",
                "title": "Missing empty-input guard",
                "detail": "handle() panics when input slice is empty.",
                "sources": ["claude-1", "codex-1"],
            }]),
        ),
    };
    let r1_submit = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: sup_exec_id,
            work_item_id: &root_id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &serde_json::to_string(&r1_verdict_payload).unwrap(),
            idempotency_key: "verdict-r1",
        })
        .unwrap()
        .unwrap();
    let rev1_id = apply_verdict(&db, &r1_submit.proposal.id, PreMergeTargetFreshness::Current)
        .expect("round 1 verdict mints revision 1");
    assert_eq!(
        db.get_task_review_cycle_state(&root_id).unwrap(),
        (1, Some("sha-r1".to_owned()))
    );

    // Prior-round loader must surface round 1's structured finding for round 2.
    let prior_for_r2 = db
        .latest_prior_pre_merge_review_round(&root_id, pr_url, None)
        .unwrap()
        .expect("prior round 1 loaded");
    assert_eq!(prior_for_r2.head_sha.as_deref(), Some("sha-r1"));
    assert_eq!(prior_for_r2.summary.as_deref(), Some("Round 1 found missing guard."));
    assert_eq!(prior_for_r2.findings.len(), 1);
    assert_eq!(prior_for_r2.findings[0].id.as_deref(), Some("C1"));
    assert_eq!(prior_for_r2.findings[0].title, "Missing empty-input guard");

    // Round 2: revision 1 completes -> one-member delta batch created.
    assert!(should_use_delta_rereview(&flags, &db, &rev1_id));
    let r2_input = ReviewBatchCreateInput::builder()
        .cycle_root_id(root_id.clone())
        .base_sha("base-0")
        .classification(test_classification(ReviewProfile::Standard))
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(4201)
        .pr_url(pr_url)
        .target_sha("sha-r2")
        .legacy_task_id(rev1_id.clone())
        .single_reviewer_light(true)
        .build();
    let (r2_batch, r2_execs) = match db.create_pre_merge_review_batch(r2_input, REPO).unwrap() {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected round 2 solo batch, got {other:?}"),
    };
    assert!(r2_batch.single_reviewer);
    assert_eq!(r2_execs.len(), 1);
    let solo_exec_id = &r2_execs[0].id;

    // A solo pre-merge batch must reject `review_report` (it submits `review_verdict` directly).
    let wrong_report_payload = ReviewReportProposalPayload {
        batch_id: r2_batch.id.clone(),
        target_sha: "sha-r2".to_owned(),
        report: serde_json::json!({
            "batch_id": r2_batch.id,
            "pr_url": pr_url,
            "target_sha": "sha-r2",
            "phase": "pre_merge",
            "summary": "wrong verb",
            "coverage": {
                "files_inspected": [],
                "files_omitted": [],
                "limitations": [],
            },
            "findings": []
        }),
    };
    let rejected_report = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: solo_exec_id,
            work_item_id: &root_id,
            kind: ProposalKind::ReviewReport,
            payload_json: &serde_json::to_string(&wrong_report_payload).unwrap(),
            idempotency_key: "wrong-report-r2",
        })
        .unwrap()
        .unwrap();
    assert_eq!(rejected_report.proposal.state, ProposalState::Rejected);
    assert!(
        rejected_report
            .proposal
            .decision_reason
            .as_deref()
            .unwrap_or("")
            .contains("review-verdict"),
        "rejection must steer caller to review-verdict: {:?}",
        rejected_report.proposal.decision_reason
    );

    // Solo reviewer submits `review_verdict` directly: collecting -> applying, NO supervisor!
    let r2_verdict_payload = ReviewVerdictProposalPayload {
        batch_id: r2_batch.id.clone(),
        verdict: build_verdict_json(
            &r2_batch.id,
            pr_url,
            "sha-r2",
            "C1: resolved; delta introduced a duplicate helper.",
            true,
            serde_json::json!([{
                "id": "C1",
                "severity": "medium",
                "category": "duplication",
                "confidence": "high",
                "file": "src/util_copy.rs",
                "location": "fn parse_key, ~L12",
                "title": "Duplicate helper left alongside extracted module",
                "detail": "src/util_copy.rs duplicates src/util.rs::parse_key.",
                "sources": ["claude"],
            }]),
        ),
    };
    let r2_submit = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: solo_exec_id,
            work_item_id: &root_id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &serde_json::to_string(&r2_verdict_payload).unwrap(),
            idempotency_key: "verdict-r2",
        })
        .unwrap()
        .unwrap();
    assert_eq!(r2_submit.proposal.state, ProposalState::Proposed);
    assert_eq!(
        r2_submit.review_batch_quorum_outcome,
        Some(ReviewBatchQuorumOutcome::Applying)
    );
    assert_eq!(
        db.review_batch(&r2_batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Applying,
        "solo pre-merge verdict must move batch directly from collecting to applying"
    );
    assert!(
        db.review_batch_members(&r2_batch.id)
            .unwrap()
            .iter()
            .all(|m| m.role != ReviewBatchMemberRole::Supervisor),
        "solo delta batch must never create a supervisor member"
    );

    let rev2_id = apply_verdict(&db, &r2_submit.proposal.id, PreMergeTargetFreshness::Current)
        .expect("round 2 delta verdict mints revision 2");
    assert_eq!(
        db.review_batch(&r2_batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Completed
    );
    assert_eq!(
        db.get_task_review_cycle_state(&root_id).unwrap(),
        (2, Some("sha-r2".to_owned())),
        "cycle count must increment seamlessly from round 1 (fanout) to round 2 (delta)"
    );

    // Round 3's prior-round loader now sees round 2's structured finding!
    let prior_for_r3 = db
        .latest_prior_pre_merge_review_round(&root_id, pr_url, None)
        .unwrap()
        .expect("prior round 2 loaded");
    assert_eq!(prior_for_r3.head_sha.as_deref(), Some("sha-r2"));
    assert_eq!(prior_for_r3.findings.len(), 1);
    assert_eq!(
        prior_for_r3.findings[0].title,
        "Duplicate helper left alongside extracted module"
    );

    // Round 3 with max_review_cycles = 2 must trip the cycle cap and record skipped_cycle_cap.
    let rev2_exec = start_running_execution(&db, &rev2_id, ExecutionKind::RevisionImplementation, workspace.path());
    let r3_enqueuer = ModeAwareStubEnqueuer::new("sha-r3");
    let verifier = StubBranchVerifier::ok("boss/test");
    *verifier.fresh_head_oid_result.try_lock().unwrap() = Ok("sha-r3".to_owned());
    *verifier.head_oid_result.try_lock().unwrap() = Ok("sha-r3".to_owned());
    let handler_r3 = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_enable_revision_triggered_reviews(true)
        .with_max_review_cycles(2)
        .with_feature_flags(flags.clone())
        .with_branch_verifier(verifier)
        .with_merge_probe(Arc::new(HeadProbe("sha-r3".to_owned())))
        .with_review_batch_enqueuer(r3_enqueuer.clone());
    let outcome = handler_r3
        .finalize_pr_transition(
            &rev2_exec,
            pr_url.to_owned(),
            WorkerPrCompletionTarget::InReview,
            "test-r3-cap",
        )
        .await;
    assert_eq!(
        outcome,
        StopOutcome::PrDetected {
            pr_url: pr_url.to_owned()
        }
    );
    assert_eq!(
        r3_enqueuer.calls.load(Ordering::SeqCst),
        0,
        "cycle cap must prevent round 3 from admitting another review batch"
    );
    let verdicts = db.review_verdicts_for_work_item(&root_id).unwrap();
    assert_eq!(verdicts[0].gate_outcome, REVIEW_GATE_OUTCOME_SKIPPED_CYCLE_CAP);
}

/// Stale-head and duplicate-head guards read the exact same state on one-member
/// delta batches as on full fanout batches.
#[test]
fn solo_delta_batch_enforces_stale_head_and_duplicate_head_guards() {
    let (_db_dir, db) = open_db_arc();
    let product = create_test_product(&db);
    let pr_url = "https://github.com/spinyfin/mono/pull/4301";
    let root_id = create_chore_in_arm(&db, &product.id, pr_url, true);

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', review_cycle = 1, last_reviewed_sha = 'sha-r1' WHERE id = ?1",
            [&root_id],
        )
        .unwrap();

    // Case 1: Stale-head guard (live head moved from sha-r2 to sha-r3 before apply).
    let stale_input = ReviewBatchCreateInput::builder()
        .cycle_root_id(root_id.clone())
        .base_sha("base-0")
        .classification(test_classification(ReviewProfile::Light))
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(4301)
        .pr_url(pr_url)
        .target_sha("sha-r2")
        .single_reviewer_light(true)
        .build();
    let (stale_batch, stale_execs) = match db.create_pre_merge_review_batch(stale_input, REPO).unwrap() {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected solo batch, got {other:?}"),
    };
    let stale_payload = ReviewVerdictProposalPayload {
        batch_id: stale_batch.id.clone(),
        verdict: build_verdict_json(
            &stale_batch.id,
            pr_url,
            "sha-r2",
            "Finding on superseded head.",
            true,
            serde_json::json!([{
                "id": "C1",
                "severity": "high",
                "category": "correctness",
                "confidence": "high",
                "file": "src/lib.rs",
                "location": "L10",
                "title": "Stale bug",
                "detail": "Bug on old head.",
                "sources": ["claude"],
            }]),
        ),
    };
    let stale_submit = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &stale_execs[0].id,
            work_item_id: &root_id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &serde_json::to_string(&stale_payload).unwrap(),
            idempotency_key: "stale-verdict",
        })
        .unwrap()
        .unwrap();
    let minted = apply_verdict(
        &db,
        &stale_submit.proposal.id,
        PreMergeTargetFreshness::SupersededBy("sha-r3".to_owned()),
    );
    assert!(minted.is_none(), "stale head must not mint a revision");
    assert_eq!(
        db.get_task_review_cycle_state(&root_id).unwrap(),
        (1, Some("sha-r1".to_owned())),
        "stale head must not increment review_cycle or advance last_reviewed_sha"
    );
    let verdicts = db.review_verdicts_for_work_item(&root_id).unwrap();
    assert_eq!(verdicts[0].gate_outcome, "stale_head");

    // Case 2: Duplicate-head guard (verdict applied after sha-r3 is already reviewed).
    let dup_input = ReviewBatchCreateInput::builder()
        .cycle_root_id(root_id.clone())
        .base_sha("base-0")
        .classification(test_classification(ReviewProfile::Light))
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(4301)
        .pr_url(pr_url)
        .target_sha("sha-r3")
        .single_reviewer_light(true)
        .build();
    let (dup_batch, dup_execs) = match db.create_pre_merge_review_batch(dup_input, REPO).unwrap() {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected solo batch, got {other:?}"),
    };
    let dup_payload = ReviewVerdictProposalPayload {
        batch_id: dup_batch.id.clone(),
        verdict: build_verdict_json(
            &dup_batch.id,
            pr_url,
            "sha-r3",
            "Duplicate head verdict.",
            true,
            serde_json::json!([{
                "id": "C1",
                "severity": "high",
                "category": "correctness",
                "confidence": "high",
                "file": "src/lib.rs",
                "location": "L10",
                "title": "Duplicate bug",
                "detail": "Already reviewed.",
                "sources": ["claude"],
            }]),
        ),
    };
    let dup_submit = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &dup_execs[0].id,
            work_item_id: &root_id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &serde_json::to_string(&dup_payload).unwrap(),
            idempotency_key: "dup-verdict",
        })
        .unwrap()
        .unwrap();
    // Simulate sha-r3 already recorded as reviewed before this verdict applies.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET review_cycle = 2, last_reviewed_sha = 'sha-r3' WHERE id = ?1",
            [&root_id],
        )
        .unwrap();
    let minted = apply_verdict(&db, &dup_submit.proposal.id, PreMergeTargetFreshness::Current);
    assert!(minted.is_none(), "duplicate head must not mint a revision");
    assert_eq!(
        db.get_task_review_cycle_state(&root_id).unwrap(),
        (2, Some("sha-r3".to_owned())),
        "duplicate head must not bump review_cycle"
    );
    let verdicts = db.review_verdicts_for_work_item(&root_id).unwrap();
    assert_eq!(verdicts[0].gate_outcome, "dropped_duplicate_head");
}

/// Post-merge review eligibility reads the newest PreMerge `pr_review_batches`
/// row's `classification.profile == Deep`. A PR that grows past the Deep
/// threshold on a round-2 delta batch must still persist `Deep` on the batch
/// row while dispatching its single member at the Light tier (`sonnet`).
#[tokio::test]
async fn solo_delta_batch_persists_whole_pr_deep_classification_for_post_merge_eligibility() {
    let (_db_dir, db) = open_db_arc();
    let flags_dir = tempdir().unwrap();
    let flags = delta_flags(flags_dir.path());
    let product = create_test_product(&db);
    let pr_url = "https://github.com/spinyfin/mono/pull/4401";
    let root_id = create_chore_in_arm(&db, &product.id, pr_url, true);

    // Round 1 was Standard (below Deep's 1000-line threshold).
    let r1_enqueuer = ModeAwareStubEnqueuer::with_diff_size("sha-r1", 500, 100);
    let r1_dispatch = r1_enqueuer.enqueue(&db, &root_id, REPO, pr_url, 16).await.unwrap();
    let r1_batch = match r1_dispatch {
        ReviewBatchDispatch::Created { batch, .. } => batch,
        other => panic!("expected round 1 batch, got {other:?}"),
    };
    assert_eq!(r1_batch.classification.profile, ReviewProfile::Standard);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'completed' WHERE id = ?1",
            [&r1_batch.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET review_cycle = 1, last_reviewed_sha = 'sha-r1' WHERE id = ?1",
            [&root_id],
        )
        .unwrap();

    // Round 2 grows past 1000 lines: classified Deep for post-merge eligibility,
    // yet dispatched as a single Light-tier (`sonnet`) delta reviewer!
    let r2_enqueuer = ModeAwareStubEnqueuer::with_diff_size("sha-r2", 950, 250);
    let r2_dispatch = r2_enqueuer
        .enqueue_with_mode(
            &db,
            &root_id,
            REPO,
            pr_url,
            16,
            should_use_delta_rereview(&flags, &db, &root_id),
        )
        .await
        .unwrap();
    let (r2_batch, r2_execs) = match r2_dispatch {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected round 2 solo batch, got {other:?}"),
    };
    assert!(r2_batch.single_reviewer);
    assert_eq!(r2_batch.classification.profile, ReviewProfile::Deep);
    assert_eq!(r2_execs.len(), 1);
    let members = db.review_batch_members(&r2_batch.id).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].resolved_model, "sonnet");

    // The newest PreMerge batch returned to `post_merge_review` carries Deep.
    let newest_pre_merge = db
        .review_batches_for_cycle_root(&root_id)
        .unwrap()
        .into_iter()
        .find(|b| b.phase == ReviewBatchPhase::PreMerge)
        .expect("newest pre-merge batch present");
    assert_eq!(newest_pre_merge.id, r2_batch.id);
    assert_eq!(newest_pre_merge.classification.profile, ReviewProfile::Deep);
}

/// Retry and failure lifecycle for a one-member delta batch:
/// attempt 1 dies -> retried once as `ClaudeReviewer` (`sonnet`) -> attempt 2
/// dies -> listed by dead-member recovery and fails the batch with attention,
/// never dispatching a supervisor.
#[test]
fn solo_delta_batch_retries_once_at_light_tier_then_fails_with_attention() {
    let (_db_dir, db) = open_db_arc();
    let product = create_test_product(&db);
    let pr_url = "https://github.com/spinyfin/mono/pull/4501";
    let root_id = create_chore_in_arm(&db, &product.id, pr_url, true);
    db.update_work_item(
        &root_id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let input = ReviewBatchCreateInput::builder()
        .cycle_root_id(root_id.clone())
        .base_sha("base-0")
        .classification(test_classification(ReviewProfile::Light))
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(4501)
        .pr_url(pr_url)
        .target_sha("sha-r2")
        .single_reviewer_light(true)
        .build();
    let (batch, execs) = match db.create_pre_merge_review_batch(input, REPO).unwrap() {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected solo batch, got {other:?}"),
    };
    let attempt1_exec = &execs[0].id;

    // Attempt 1 dies (orphaned without Stop hook).
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned' WHERE id = ?1",
            [attempt1_exec],
        )
        .unwrap();
    let dead1 = db.list_dead_review_batch_member_candidates().unwrap();
    assert!(dead1.iter().any(|c| c.execution_id == *attempt1_exec));

    let attempt2_exec = match db.retry_dead_review_batch_member(attempt1_exec).unwrap() {
        RetryDeadReviewBatchMember::Retried(exec) => exec,
        other => panic!("expected retry on attempt 1, got {other:?}"),
    };
    let members = db.review_batch_members(&batch.id).unwrap();
    assert_eq!(members.len(), 2);
    let m2 = members
        .iter()
        .find(|m| m.attempt == 2)
        .expect("attempt 2 member inserted");
    assert_eq!(m2.role, ReviewBatchMemberRole::ClaudeReviewer);
    assert_eq!(m2.resolved_model, "sonnet");
    assert_eq!(m2.provider_effort, "medium");
    assert_eq!(m2.status, ReviewBatchMemberStatus::Pending);

    // Attempt 2 also dies without Stop hook -> recovery must list it and fail the batch.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned' WHERE id = ?1",
            [&attempt2_exec.id],
        )
        .unwrap();
    let dead2 = db.list_dead_review_batch_member_candidates().unwrap();
    assert!(
        dead2.iter().any(|c| c.execution_id == attempt2_exec.id),
        "attempt 2 of a solo delta batch must be listed so recovery can fail the batch"
    );
    assert!(matches!(
        db.retry_dead_review_batch_member(&attempt2_exec.id).unwrap(),
        RetryDeadReviewBatchMember::BatchFailed
    ));
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Failed
    );
    assert!(
        db.review_batch_members(&batch.id)
            .unwrap()
            .iter()
            .all(|m| m.role != ReviewBatchMemberRole::Supervisor),
        "failed solo batch must never create a supervisor"
    );
    // Once failed, it drops out of dead-member candidates.
    assert!(db.list_dead_review_batch_member_candidates().unwrap().is_empty());
}

/// A one-member delta batch reserves 1 review-pool unit (instead of 4 for a
/// full fanout batch), so 4 solo batches fit in a 4-unit pool and a 5th defers.
#[test]
fn solo_delta_batch_reserves_one_pool_unit() {
    let (_db_dir, db) = open_db_arc();
    let product = create_test_product(&db);
    let pool_size = 4;

    for idx in 0..4 {
        let pr_url = format!("https://github.com/spinyfin/mono/pull/{}", 4600 + idx);
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name(format!("solo-pool-{idx}"))
                    .description("Solo pool test")
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let input = ReviewBatchCreateInput::builder()
            .cycle_root_id(chore.id)
            .base_sha("base-0")
            .classification(test_classification(ReviewProfile::Light))
            .phase(ReviewBatchPhase::PreMerge)
            .pr_number(4600 + idx)
            .pr_url(&pr_url)
            .target_sha(format!("sha-{idx}"))
            .single_reviewer_light(true)
            .build();
        assert!(
            matches!(
                db.create_pre_merge_review_batch_for_pool(input, REPO, pool_size)
                    .unwrap(),
                ReviewBatchDispatch::Created { .. }
            ),
            "solo batch #{idx} must fit in a 4-unit pool"
        );
    }

    // The 5th solo batch must defer because all 4 pool units are reserved.
    let fifth_chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id)
                .name("solo-pool-5")
                .description("Fifth solo batch")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let fifth_input = ReviewBatchCreateInput::builder()
        .cycle_root_id(fifth_chore.id)
        .base_sha("base-0")
        .classification(test_classification(ReviewProfile::Light))
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(4604)
        .pr_url("https://github.com/spinyfin/mono/pull/4604")
        .target_sha("sha-4")
        .single_reviewer_light(true)
        .build();
    assert!(matches!(
        db.create_pre_merge_review_batch_for_pool(fifth_input, REPO, pool_size)
            .unwrap(),
        ReviewBatchDispatch::AdmissionDeferred
    ));
}
