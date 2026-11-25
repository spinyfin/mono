//! Tests for the `pr_created_proposals_seam` proposal-first completion path.
//! Test functions only; shared fixtures, stubs, and helpers live
//! in the parent [`super`] module (`completion/tests.rs`).
//!
//! Theme: PR-created-declaration worker-proposal seam
//! (`pr_created_proposals_seam`). Mirrors the `automation_outcome_proposals_seam`
//! tests in `t11.rs`, adapted to the PR-detection ladder's shape: instead of
//! one `finalize_*` entry point, `pr_created_from_proposal` is consulted from
//! both `on_stop_inner` and `recheck_for_pr`, and every OTHER source that
//! reaches the shared `finalize_pr_transition` funnel counts as a fallback.

use super::*;

fn enable_pr_created_seam() -> (Arc<crate::feature_flags::FeatureFlagsStore>, TempDir) {
    let flags_dir = tempdir().unwrap();
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        flags_dir.path().join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("worker_proposals", true).unwrap();
    flags.set("pr_created_proposals_seam", true).unwrap();
    crate::review_guide_capture::pin_review_guide_flags_off(&flags);
    (flags, flags_dir)
}

fn enable_pr_created_and_run_done_seams() -> (Arc<crate::feature_flags::FeatureFlagsStore>, TempDir) {
    let (flags, flags_dir) = enable_pr_created_seam();
    flags.set("run_done_proposals_seam", true).unwrap();
    (flags, flags_dir)
}

/// `source` values on `finalize_pr_transition`'s terminalize/teardown log
/// for `execution_id`. SHA-delta and the proposal arm produce the same URL,
/// detector-zero, fallback-zero, and head-stamp outcomes for a matching-URL
/// revision, so tests that must prove `stop_proposal` inspect this instead.
fn terminalized_finalize_sources(captured: &str, execution_id: &str) -> Vec<String> {
    captured
        .lines()
        .filter(|line| {
            line.contains(execution_id) && line.contains("pr completion: execution terminalized; teardown in flight")
        })
        .filter_map(|line| {
            line.split_whitespace().find_map(|tok| {
                tok.strip_prefix("source=")
                    .map(|value| value.trim_matches('"').to_owned())
            })
        })
        .collect()
}

/// Proposal path wins over the cold-reconstruction ladder — and, because
/// this test never populates `StagedPrUrlCache` (the default empty cache,
/// exactly what an engine restart between the worker's `cube pr create` and
/// this Stop would leave behind), it also demonstrates the seam's durability
/// win: finalization succeeds from the durable `pr_created` proposal row
/// alone, with no in-memory staging evidence at all.
#[tokio::test]
async fn pr_created_proposals_first_uses_proposal_url_surviving_an_empty_staging_cache() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::PrCreated,
        payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();

    // If the ladder ran at all, the detector would supply THIS (different)
    // URL — proves the proposal, not the cold path, decided the outcome.
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/999"));

    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/500"),
        "expected finalization via the proposal's URL, got {outcome:?}",
    );
    assert_eq!(
        detector.call_count(),
        0,
        "the pr_created proposal must short-circuit the cold-reconstruction ladder entirely",
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => assert_eq!(t.pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/500")),
        other => panic!("expected chore, got {other:?}"),
    }
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "the proposal covered the URL; the legacy ladder must never fire",
    );
}

/// No `pr_created` proposal exists: the legacy staging-cache / cold-
/// reconstruction ladder still runs exactly as before, and the fallback hit
/// is counted — this seam's explicit exit criterion.
#[tokio::test]
async fn pr_created_proposals_first_falls_back_to_the_ladder_and_counts_the_hit() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/12"));

    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/12"),
        "expected the legacy ladder to still finalize via the detector, got {outcome:?}",
    );
    assert_eq!(
        detector.call_count(),
        1,
        "no proposal existed — the cold detector must run"
    );
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => assert_eq!(t.pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/12")),
        other => panic!("expected chore, got {other:?}"),
    }
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(1),
        "no proposal existed, so the legacy ladder fired and must count as a fallback hit",
    );
}

/// Even with an existing `pr_created` proposal present, the flag defaulting
/// off must reproduce the exact pre-seam behavior: the legacy ladder always
/// decides, the proposal is never consulted, and nothing is counted.
#[tokio::test]
async fn pr_created_proposals_first_flag_off_matches_pre_migration_behavior_exactly() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::PrCreated,
        payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    // Distinct from the proposal's URL, so a wrongly-proposal-first path
    // would produce a detectably different (wrong) answer.
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/12"));

    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/12"),
        "flag off: must decide via the legacy ladder, not the proposal, got {outcome:?}",
    );
    assert_eq!(detector.call_count(), 1);
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "with the flag off nothing is counted",
    );
}

/// Merge-poller mirror: `recheck_for_pr` also reads the `pr_created`
/// proposal before its own staging-cache / SHA-delta / cold-reconstruction
/// chain, and skips the detector entirely when a proposal covers it.
#[tokio::test]
async fn pr_created_proposals_first_via_recheck_for_pr_uses_proposal_and_skips_detector() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::PrCreated,
        payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/999"));

    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    let outcome = handler.recheck_for_pr(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/500"),
        "expected pr-recheck to finalize via the proposal's URL, got {outcome:?}",
    );
    assert_eq!(
        detector.call_count(),
        0,
        "the proposal must short-circuit the cold detector on recheck too"
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "the proposal covered the URL; the legacy ladder must never fire",
    );
}

/// The durable proposal path has the same mid-turn safety boundary as the
/// staged-URL path: a merge-poller sweep must leave a working revision for
/// its own Stop boundary to finish PR reconciliation and review reporting.
///
/// Deferral keys on the Applied row's `created_at` and `observed_mid_turn`,
/// not the in-memory staging cache (empty after an engine restart — the
/// case this arm exists for). The test ages `last_event_at` past the
/// horizon and stubs a moved head so `finalize_pr_transition`'s mid-turn
/// guard and contribution gate would both allow; if the arm-level check
/// were removed, recheck would finalize.
#[tokio::test]
async fn pr_created_proposal_recheck_defers_a_mid_turn_revision() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/500";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &revision_id,
        kind: ProposalKind::PrCreated,
        payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();

    let live_states = Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
    live_states.register_spawn(
        2,
        &execution_id,
        "claude-opus-5",
        std::process::id() as i32,
        Some(boss_protocol::WorkItemBinding {
            work_item_id: revision_id.clone(),
            work_item_name: "proposal mid-turn revision".to_owned(),
            execution_id: execution_id.clone(),
        }),
    );
    live_states.apply_event(
        2,
        &boss_protocol::WorkerEvent::PreToolUse {
            session_id: "s".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
        },
    );
    // Age the activity horizon so finalize_pr_transition would not defer.
    // The arm-level check still keys on the proposal row's created_at.
    live_states.set_last_event_at_for_test(2, "2000-01-01T00:00:00Z");

    let verifier = StubBranchVerifier::ok("boss/test");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned()))
        .await;

    let (flags, _flags_dir) = enable_pr_created_seam();
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, cube, .. } = TestHarness::new(db.clone(), detector.clone());
    let outcome = handler
        .with_feature_flags(flags)
        .with_live_worker_states(live_states)
        .with_staged_pr_mid_turn_defer_secs(60)
        .with_branch_verifier(verifier.clone())
        .recheck_for_pr(&execution_id)
        .await;

    assert_eq!(outcome, StopOutcome::AwaitingInput);
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(task) => assert_eq!(task.status, TaskStatus::Active),
        other => panic!("expected revision task, got {other:?}"),
    }
    assert!(cube.release_calls.lock().await.is_empty());
    assert_eq!(
        detector.call_count(),
        0,
        "arm-level deferral must return before the ladder"
    );
    assert!(
        verifier.head_oid_calls_snapshot().is_empty(),
        "arm-level deferral must return before contribution evaluation fetches the PR head"
    );
}

/// A `pr_created` proposal rejected at submission (wrong repo/URL shape)
/// carries no usable URL — the legacy ladder runs as the counted fallback,
/// exactly like the "no proposal at all" case.
#[tokio::test]
async fn pr_created_proposals_first_rejected_proposal_falls_back_to_the_ladder() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    // A PR URL from an unrelated repo fails `validate_pr_url`'s product-repo
    // gate inside `apply_pr_created` — the proposal is submitted but ends up
    // `Rejected`, not `Applied`.
    let rejected = db
        .submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
            execution_id: &execution_id,
            work_item_id: &chore_id,
            kind: ProposalKind::PrCreated,
            payload_json: r#"{"pr_url":"https://github.com/some-other-org/other-repo/pull/1"}"#,
            idempotency_key: "key-1",
        })
        .unwrap()
        .unwrap();
    assert_eq!(rejected.proposal.state, ProposalState::Rejected);

    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/12"));
    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/12"),
        "a rejected proposal carries no usable URL; expected the legacy ladder to decide, got {outcome:?}",
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(1),
        "a rejected proposal still counts as 'the proposal did not cover this finalization'",
    );
}

/// Completion paths that act on an already-bound PR are outside the
/// PR-creation ladder. They must not affect this seam's deletion criterion.
#[tokio::test]
async fn pr_created_fallback_counter_ignores_non_ladder_finalization_sources() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db, StubPrDetector::ok(None));
    let handler = handler.with_feature_flags(flags).with_metrics(metrics.clone());

    let outcome = handler
        .finalize_pr_transition(
            &execution_id,
            "https://github.com/spinyfin/mono/pull/500".to_owned(),
            WorkerPrCompletionTarget::InReview,
            "metadata_only_fix",
        )
        .await;
    assert!(matches!(outcome, StopOutcome::ReviewerEnqueued { .. }));
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "an already-bound-PR completion source is not a missing pr_created declaration",
    );
}

/// Revision declarations are accepted durably without mutating the revision
/// task row, but only the chain root's bound PR may be used at completion.
#[tokio::test]
async fn pr_created_proposal_for_revision_rejects_a_url_other_than_the_bound_chain_root_pr() {
    let workspace = tempdir().unwrap();
    let bound_pr_url = "https://github.com/spinyfin/mono/pull/500";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(
        workspace.path(),
        bound_pr_url,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    // The applier's revision no-stamp hardening is gated on the seam too
    // (see `apply_pr_created`'s doc) — call `submit_worker_proposal_with_flags`
    // directly with it on, so the submission this test makes actually
    // exercises the hardened path it asserts on. `WorkDb` has no
    // `FeatureFlagsStore` of its own; the flag's live value is always
    // supplied by the caller that resolved it.
    let submitted = db
        .submit_worker_proposal_with_flags(
            crate::work::SubmitWorkerProposalInput {
                execution_id: &execution_id,
                work_item_id: &revision_id,
                kind: ProposalKind::PrCreated,
                payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/999"}"#,
                idempotency_key: "key-1",
            },
            true,
        )
        .unwrap()
        .unwrap();
    assert_eq!(submitted.proposal.state, ProposalState::Applied);

    let (flags, _flags_dir) = enable_pr_created_seam();
    let detector = StubPrDetector::ok(None);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let outcome = handler.with_feature_flags(flags).on_stop(&execution_id).await;
    assert!(
        !matches!(
            &outcome,
            StopOutcome::ReviewerEnqueued { pr_url } | StopOutcome::PrDetected { pr_url }
                if pr_url == "https://github.com/spinyfin/mono/pull/999"
        ),
        "a revision may not finalize against a declaration other than its bound chain-root PR, got {outcome:?}",
    );
    let revision = match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected revision task, got {other:?}"),
    };
    assert!(revision.pr_url.is_none(), "revision tasks never own a PR URL");
}

/// With `run_done_proposals_seam` on, an Applied `pr_created` row is not
/// enough to finalize a primary execution: a `delivered` declaration is
/// still required, matching the staged-URL arm this proposal arm
/// short-circuits.
#[tokio::test]
async fn pr_created_proposal_without_run_done_leaves_primary_execution_live_on_stop() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::PrCreated,
        payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();

    let (flags, _flags_dir) = enable_pr_created_and_run_done_seams();
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler
        .with_feature_flags(flags)
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::AwaitingRunDoneDeclaration { .. }),
        "an undeclared primary pr_created proposal must be held pending declaration, not finalized; got {outcome:?}",
    );
    let execution = db.get_execution(&execution_id).unwrap();
    assert!(
        execution.status.is_live(),
        "an undeclared run must stay live when only a pr_created proposal is present"
    );
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected chore, got {other:?}"),
    }
}

/// Merge-poller mirror of the Stop-path declaration gate.
#[tokio::test]
async fn pr_created_proposal_without_run_done_leaves_primary_execution_live_on_recheck() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::PrCreated,
        payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();

    let (flags, _flags_dir) = enable_pr_created_and_run_done_seams();
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler
        .with_feature_flags(flags)
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    let outcome = handler.recheck_for_pr(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::AwaitingRunDoneDeclaration { .. }),
        "an undeclared primary pr_created proposal must be held pending declaration on recheck; got {outcome:?}",
    );
    let execution = db.get_execution(&execution_id).unwrap();
    assert!(
        execution.status.is_live(),
        "an undeclared run must stay live on the poller sweep"
    );
}

/// A `delivered` run_done declaration lets the proposal-first arm complete
/// the same primary execution that the undeclared case holds.
#[tokio::test]
async fn pr_created_proposal_with_delivered_run_done_finalizes_on_stop() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::PrCreated,
        payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::RunDone,
        payload_json: r#"{"outcome":"delivered","summary":"opened the PR"}"#,
        idempotency_key: "done-1",
    })
    .unwrap()
    .unwrap();

    let (flags, _flags_dir) = enable_pr_created_and_run_done_seams();
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let handler = handler
        .with_feature_flags(flags)
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/500"),
        "a delivered declaration must allow proposal-first completion, got {outcome:?}",
    );
    let execution = db.get_execution(&execution_id).unwrap();
    assert!(
        !execution.status.is_live(),
        "a delivered run must be finalized from the pr_created proposal"
    );
}

/// A predecessor execution on the same work item that already opened the PR
/// (the resume / bounce-back shape: the new execution is told to
/// `cube pr update` the predecessor's branch).
fn add_predecessor_execution(db: &WorkDb, task_id: &str) -> String {
    db.create_execution(
        boss_protocol::CreateExecutionInput::builder()
            .work_item_id(task_id)
            .kind(boss_protocol::ExecutionKind::ChoreImplementation)
            .status(boss_protocol::ExecutionStatus::Completed)
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .build(),
    )
    .unwrap()
    .id
}

/// A resumed execution whose PR lives on the predecessor's branch must
/// still finalize from the Applied `pr_created` row — the same work-item
/// branch set the staged-URL path accepts.
#[tokio::test]
async fn pr_created_proposal_accepts_a_resumed_execution_on_the_predecessor_branch() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let predecessor = add_predecessor_execution(&db, &chore_id);
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::PrCreated,
        payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();

    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/999"));
    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &predecessor,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/500"),
        "a resumed execution must finalize via the proposal on the predecessor's branch, got {outcome:?}",
    );
    assert_eq!(
        detector.call_count(),
        0,
        "the proposal must short-circuit the ladder for a predecessor-branch PR"
    );
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "a usable predecessor-branch declaration is not a fallback hit",
    );
}

/// A Rejected re-declaration must not hide an earlier Applied row: the
/// sequence "declare PR #1 (Applied) → declare PR #2 (Rejected: already
/// bound)" still has a usable URL.
#[tokio::test]
async fn pr_created_proposal_uses_an_earlier_applied_row_after_a_rejected_redeclaration() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let first = db
        .submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
            execution_id: &execution_id,
            work_item_id: &chore_id,
            kind: ProposalKind::PrCreated,
            payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
            idempotency_key: "key-1",
        })
        .unwrap()
        .unwrap();
    assert_eq!(first.proposal.state, ProposalState::Applied);
    let second = db
        .submit_worker_proposal_with_flags(
            crate::work::SubmitWorkerProposalInput {
                execution_id: &execution_id,
                work_item_id: &chore_id,
                kind: ProposalKind::PrCreated,
                payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/501"}"#,
                idempotency_key: "key-2",
            },
            true,
        )
        .unwrap()
        .unwrap();
    assert_eq!(second.proposal.state, ProposalState::Rejected);

    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/999"));
    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )));

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/500"),
        "the Applied declaration must still finalize after a later Rejected row, got {outcome:?}",
    );
    assert_eq!(detector.call_count(), 0);
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "a usable Applied row is not a fallback hit",
    );
}

/// An Applied `pr_created` URL whose PR head does not identify the work
/// item falls through to the legacy ladder, which finalizes, and the
/// fallback counter records that a row existed but could not be used.
#[tokio::test]
async fn pr_created_proposal_falls_through_when_the_pr_head_does_not_identify_the_execution() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id: &execution_id,
        work_item_id: &chore_id,
        kind: ProposalKind::PrCreated,
        payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
        idempotency_key: "key-1",
    })
    .unwrap()
    .unwrap();

    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/12"));
    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone())
        .with_branch_verifier(StubBranchVerifier::ok("boss/unrelated-other-work"));

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(&outcome, StopOutcome::ReviewerEnqueued { pr_url } if pr_url == "https://github.com/spinyfin/mono/pull/12"),
        "a branch-mismatched proposal must fall through to the ladder, got {outcome:?}",
    );
    assert_eq!(detector.call_count(), 1);
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(1),
        "a row existed but could not be used: the ladder hit must count",
    );
}

/// A revision declaring its chain-root PR stamps `revision_stop_contributed_head`
/// and finalizes through the proposal arm.
#[tokio::test]
async fn pr_created_proposal_for_revision_matching_bound_pr_finalizes_on_stop() {
    let workspace = tempdir().unwrap();
    let bound_pr_url = "https://github.com/spinyfin/mono/pull/500";
    let (_dir, db, _product_id, revision_id, execution_id) = revision_fixture(
        workspace.path(),
        bound_pr_url,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    db.submit_worker_proposal_with_flags(
        crate::work::SubmitWorkerProposalInput {
            execution_id: &execution_id,
            work_item_id: &revision_id,
            kind: ProposalKind::PrCreated,
            payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
            idempotency_key: "key-1",
        },
        true,
    )
    .unwrap()
    .unwrap();

    let detector = StubPrDetector::ok(Some("https://github.com/spinyfin/mono/pull/999"));
    let (flags, _flags_dir) = enable_pr_created_seam();
    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let verifier = StubBranchVerifier::ok("boss/test");
    verifier
        .set_head_oid(Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned()))
        .await;
    let logs = crate::test_support::log_capture::install();
    let log_offset = logs.lock().len();
    let outcome = handler
        .with_feature_flags(flags)
        .with_metrics(metrics.clone())
        .with_branch_verifier(verifier)
        .on_stop(&execution_id)
        .await;
    assert!(
        matches!(
            &outcome,
            StopOutcome::PrDetected { pr_url } | StopOutcome::ReviewerEnqueued { pr_url } if pr_url == bound_pr_url
        ),
        "a revision declaring its chain-root PR must finalize via the proposal, got {outcome:?}",
    );
    let captured = String::from_utf8_lossy(&logs.lock()[log_offset..]).into_owned();
    assert_eq!(
        terminalized_finalize_sources(&captured, &execution_id),
        vec![crate::PR_CREATED_PROPOSAL_STOP_SOURCE.to_owned()],
        "this fixture must finalize through the proposal arm (`{}`); \
         SHA-delta would also stamp the head, skip the detector, and leave the fallback counter at 0. \
         captured:\n{captured}",
        crate::PR_CREATED_PROPOSAL_STOP_SOURCE,
    );
    assert_eq!(detector.call_count(), 0);
    assert_eq!(
        metrics.counter_value("worker_proposals.fallback_hit.pr_created"),
        Some(0),
        "proposal-arm finalization must not count as a ladder fallback",
    );
    assert!(
        db.get_revision_stop_contributed_head(&execution_id).unwrap().is_some(),
        "the proposal arm must stamp revision_stop_contributed_head before finalizing",
    );
}

/// When the contribution gate refuses a matching revision declaration,
/// Stop falls through to the SHA-delta path instead of swallowing
/// `AwaitingInput`.
#[tokio::test]
async fn pr_created_proposal_for_revision_falls_through_when_the_contribution_gate_refuses() {
    let workspace = tempdir().unwrap();
    let bound_pr_url = "https://github.com/spinyfin/mono/pull/500";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), bound_pr_url, head_before);
    db.submit_worker_proposal_with_flags(
        crate::work::SubmitWorkerProposalInput {
            execution_id: &execution_id,
            work_item_id: &revision_id,
            kind: ProposalKind::PrCreated,
            payload_json: r#"{"pr_url":"https://github.com/spinyfin/mono/pull/500"}"#,
            idempotency_key: "key-1",
        },
        true,
    )
    .unwrap()
    .unwrap();

    let verifier = StubBranchVerifier::ok("boss/test");
    verifier.set_head_oid(Ok(head_before.to_owned())).await;

    let (flags, _flags_dir) = enable_pr_created_seam();
    let TestHarness { handler, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let logs = crate::test_support::log_capture::install();
    let log_offset = logs.lock().len();
    let outcome = handler
        .with_feature_flags(flags)
        .with_branch_verifier(verifier)
        .on_stop(&execution_id)
        .await;
    assert_eq!(
        outcome,
        StopOutcome::AwaitingInput,
        "an unchanged head must refuse the proposal arm and land on the SHA-delta no-contribution nudge, got {outcome:?}",
    );
    let captured = String::from_utf8_lossy(&logs.lock()[log_offset..]).into_owned();
    assert!(
        captured.lines().any(|line| line.contains(&execution_id)
            && line.contains("proposal-URL finalize refused by revision contribution gate")),
        "the proposal arm must refuse and fall through; captured:\n{captured}",
    );
    assert!(
        terminalized_finalize_sources(&captured, &execution_id).is_empty(),
        "contribution-gate refuse must not terminalize via `{}` or any other finalize source; captured:\n{captured}",
        crate::PR_CREATED_PROPOSAL_STOP_SOURCE,
    );
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(task) => assert_eq!(task.status, TaskStatus::Active),
        other => panic!("expected revision task, got {other:?}"),
    }
}
