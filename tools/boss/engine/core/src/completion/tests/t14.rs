//! Direct coverage for the declared-delivery `PendingReview` hold taken by
//! [`crate::completion::run_done_declaration::WorkerCompletionHandler::finalize_declared_delivery`]
//! and released by
//! [`crate::completion::run_done_declaration::maybe_enqueue_declared_delivery_reviewer`],
//! plus the `recheck_for_pr` declaration gate this same revision restores.
//!
//! Every arm that decides NOT to enqueue a reviewer must still release the
//! hold it inherited — the critical bug this revision fixes was exactly a
//! release call that silently no-op'd (`let _ =`) because it required a
//! `pr_review_verdicts` row keyed by the wrong id. These tests call the free
//! function directly with a [`StubBranchVerifier`], the same pattern
//! `t13.rs` uses for `audit_declared_delivery`.

use async_trait::async_trait;
use tempfile::tempdir;

use super::*;
use crate::completion::ReviewBatchEnqueuer;
use crate::completion::run_done_declaration::maybe_enqueue_declared_delivery_reviewer;
use crate::work::{CreateExecutionInput, ExecutionStatus, ReviewBatchDispatch, ReviewVerdictInput, WorkItemPatch};
use boss_protocol::{ExecutionKind, TaskStatus};

const PR_URL: &str = "https://github.com/spinyfin/mono/pull/7000";

fn task_status(db: &WorkDb, work_item_id: &str) -> TaskStatus {
    match db.get_work_item(work_item_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => t.status,
        other => panic!("expected a task/chore, got {other:?}"),
    }
}

/// Stamp `work_item_id`'s own row `active` + `pr_url` set, and terminalize
/// every execution row already on it — the state
/// [`WorkDb::record_worker_pr_completion`] and
/// [`WorkerCompletionHandler::finish_worker_teardown`] leave behind for a
/// `WorkerPrCompletionTarget::PendingReview` write, which is the precondition
/// every release path in `maybe_enqueue_declared_delivery_reviewer` requires:
/// the release helpers' `NOT EXISTS (... status IN ('running','waiting_human') ...)`
/// guard refuses to advance while the producing execution still looks live,
/// exactly as it must in production (the release only ever runs from the
/// background post-effects spawn, after teardown already completed).
fn stamp_pending_review_hold(db: &WorkDb, work_item_id: &str, pr_url: &str) {
    db.update_work_item(
        work_item_id,
        WorkItemPatch {
            status: Some("active".into()),
            pr_url: Some(pr_url.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.force_execution_status_for_test(work_item_id, ExecutionStatus::Completed)
        .unwrap();
}

/// Insert an informative `pr_review_verdicts` row under `verdict_source_id`
/// — what `advance_pending_review_task_to_in_review_with_verdict_source`'s
/// `EXISTS` clause requires before it will release a hold. Mirrors
/// `pr_transition.rs`'s `already_reviewed_at_head_tests::insert_verdict`.
fn insert_informative_verdict(db: &WorkDb, verdict_source_id: &str, head_sha: &str) {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(verdict_source_id.to_owned())
                .kind(ExecutionKind::PrReview)
                .build(),
        )
        .unwrap();
    WorkDb::insert_review_verdict_in_tx(
        &db.connect().unwrap(),
        &execution.id,
        verdict_source_id,
        &ReviewVerdictInput {
            head_sha: Some(head_sha.to_owned()),
            findings_count: 0,
            revision_warranted: false,
            gate_outcome: crate::work::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
        },
    )
    .unwrap();
}

/// A [`ReviewBatchEnqueuer`] that must never be called — for skip-arm tests
/// that should return before ever reaching the enqueue step.
struct PanicsIfCalledEnqueuer;

#[async_trait]
impl ReviewBatchEnqueuer for PanicsIfCalledEnqueuer {
    async fn enqueue(
        &self,
        _work_db: &WorkDb,
        _work_item_id: &str,
        _repo_remote_url: &str,
        _pr_url: &str,
        _review_pool_size: usize,
    ) -> anyhow::Result<ReviewBatchDispatch> {
        panic!("reviewer enqueue must not be reached when the noop-skip / cycle-bound gate fires first");
    }
}

/// Always defers admission — used to exercise the `AdmissionDeferred` arm
/// without standing up a real review-pool occupancy fixture.
struct AlwaysDefersAdmissionEnqueuer;

#[async_trait]
impl ReviewBatchEnqueuer for AlwaysDefersAdmissionEnqueuer {
    async fn enqueue(
        &self,
        _work_db: &WorkDb,
        _work_item_id: &str,
        _repo_remote_url: &str,
        _pr_url: &str,
        _review_pool_size: usize,
    ) -> anyhow::Result<ReviewBatchDispatch> {
        Ok(ReviewBatchDispatch::AdmissionDeferred)
    }
}

fn empty_flags(path: &std::path::Path) -> std::sync::Arc<crate::feature_flags::FeatureFlagsStore> {
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        path.join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags
}

fn fanout_flags(path: &std::path::Path) -> std::sync::Arc<crate::feature_flags::FeatureFlagsStore> {
    let flags = empty_flags(path);
    flags.set("review_batch_fanout", true).unwrap();
    flags
}

/// The `sha_unchanged` no-op skip must release the hold on a non-revision
/// task — the direct case (`work_item_id == cycle_root_id`, so the single-arg
/// verdict lookup would have matched too, but the revision case below is
/// where the pre-fix `let _ = advance_pending_review_task_to_in_review(..)`
/// call silently failed).
#[tokio::test]
async fn sha_unchanged_skip_releases_the_hold_for_a_non_revision_task() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    stamp_pending_review_hold(&db, &chore_id, PR_URL);
    db.increment_task_review_cycle(&chore_id, Some("sha-prev")).unwrap();
    insert_informative_verdict(&db, &chore_id, "sha-prev");

    let publisher = std::sync::Arc::new(RecordingPublisher::default());
    let verifier = StubBranchVerifier::ok("boss/test");
    verifier.set_head_oid(Ok("sha-prev".into())).await;
    let flags_dir = tempdir().unwrap();
    let flags = empty_flags(flags_dir.path());
    let execution = db.get_execution(&execution_id).unwrap();

    maybe_enqueue_declared_delivery_reviewer(
        &db,
        publisher.as_ref(),
        verifier.as_ref(),
        &PanicsIfCalledEnqueuer,
        &flags,
        5,
        0,
        1,
        &execution,
        &chore_id,
        &execution.repo_remote_url,
        PR_URL,
    )
    .await;

    assert_eq!(
        task_status(&db, &chore_id),
        TaskStatus::InReview,
        "an sha_unchanged skip must release the PendingReview hold to in_review"
    );
}

/// The same `sha_unchanged` skip on a REVISION task: the review cycle state
/// and the informative verdict both live on the chain root (the parent
/// chore), not the revision task's own row, so the release must key its
/// `EXISTS` lookup off the cycle root while still updating the revision
/// task's own row — exactly the id split the pre-fix code got wrong.
#[tokio::test]
async fn sha_unchanged_skip_releases_the_hold_for_a_revision_whose_verdict_is_on_the_cycle_root() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/7001";
    let (_dir, db, _product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, "head-before");
    let cycle_root_id = db.review_cycle_root_id(&revision_id);
    assert_ne!(
        cycle_root_id, revision_id,
        "fixture precondition: revision must have a distinct chain root"
    );

    stamp_pending_review_hold(&db, &revision_id, parent_pr_url);
    db.increment_task_review_cycle(&cycle_root_id, Some("sha-prev"))
        .unwrap();
    insert_informative_verdict(&db, &cycle_root_id, "sha-prev");

    let publisher = std::sync::Arc::new(RecordingPublisher::default());
    let verifier = StubBranchVerifier::ok("boss/test");
    verifier.set_head_oid(Ok("sha-prev".into())).await;
    let flags_dir = tempdir().unwrap();
    let flags = empty_flags(flags_dir.path());
    let execution = db.get_execution(&execution_id).unwrap();

    maybe_enqueue_declared_delivery_reviewer(
        &db,
        publisher.as_ref(),
        verifier.as_ref(),
        &PanicsIfCalledEnqueuer,
        &flags,
        5,
        0,
        1,
        &execution,
        &revision_id,
        &execution.repo_remote_url,
        parent_pr_url,
    )
    .await;

    assert_eq!(
        task_status(&db, &revision_id),
        TaskStatus::InReview,
        "the revision's OWN row must advance to in_review even though the informative verdict \
         lives on the cycle root, not the revision task itself"
    );
}

/// The cycle-bound arm must also release the hold (it always runs after a
/// prior review cycle, so an informative verdict is available under the same
/// id the noop-skip arm above uses).
#[tokio::test]
async fn cycle_bound_releases_the_hold_and_files_an_attention_item() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    stamp_pending_review_hold(&db, &chore_id, PR_URL);
    db.increment_task_review_cycle(&chore_id, Some("sha-prev")).unwrap();
    insert_informative_verdict(&db, &chore_id, "sha-prev");

    let publisher = std::sync::Arc::new(RecordingPublisher::default());
    let verifier = StubBranchVerifier::ok("boss/test");
    // Head moved and the diff is non-trivial, so the noop-skip gate must NOT
    // fire — this test is specifically about the cycle-bound arm below it.
    verifier.set_head_oid(Ok("sha-new".into())).await;
    verifier.set_diff_line_count(Ok(999)).await;
    let flags_dir = tempdir().unwrap();
    let flags = empty_flags(flags_dir.path());
    let execution = db.get_execution(&execution_id).unwrap();

    maybe_enqueue_declared_delivery_reviewer(
        &db,
        publisher.as_ref(),
        verifier.as_ref(),
        &PanicsIfCalledEnqueuer,
        &flags,
        1, // max_review_cycles: already at review_cycle=1, so the bound is reached
        0,
        1,
        &execution,
        &chore_id,
        &execution.repo_remote_url,
        PR_URL,
    )
    .await;

    assert_eq!(
        task_status(&db, &chore_id),
        TaskStatus::InReview,
        "cycle bound reached must release the PendingReview hold to in_review"
    );
    let attentions = db.list_attention_items_for_work_item(&chore_id).unwrap();
    assert!(
        attentions.iter().any(|a| a.kind == "pr_review_cycle_bound"),
        "a pr_review_cycle_bound attention item must exist; got: {attentions:?}"
    );
}

/// `AdmissionDeferred` must leave the hold exactly where it is — a live
/// batch may still complete against the current head — rather than
/// releasing it like the no-enqueue arms above.
#[tokio::test]
async fn admission_deferred_leaves_the_hold_in_place() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    stamp_pending_review_hold(&db, &chore_id, PR_URL);
    // First review (review_cycle == 0): the noop-skip gate returns
    // immediately without any network call, and the cycle bound is nowhere
    // near reached — this test is purely about the AdmissionDeferred arm.

    let publisher = std::sync::Arc::new(RecordingPublisher::default());
    let verifier = StubBranchVerifier::ok("boss/test");
    let flags_dir = tempdir().unwrap();
    let flags = fanout_flags(flags_dir.path());
    let execution = db.get_execution(&execution_id).unwrap();

    maybe_enqueue_declared_delivery_reviewer(
        &db,
        publisher.as_ref(),
        verifier.as_ref(),
        &AlwaysDefersAdmissionEnqueuer,
        &flags,
        5,
        0,
        1,
        &execution,
        &chore_id,
        &execution.repo_remote_url,
        PR_URL,
    )
    .await;

    assert_eq!(
        task_status(&db, &chore_id),
        TaskStatus::Active,
        "a deferred admission must leave the task held in PendingReview (active + pr_url), not \
         advance it to in_review"
    );
    let attentions = db.list_attention_items_for_work_item(&chore_id).unwrap();
    assert!(
        attentions
            .iter()
            .any(|a| a.kind == crate::work::PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND),
        "a deferred admission must file the deferred-admission marker; got: {attentions:?}"
    );
}

/// Regression test for the recheck-vs-Stop declaration-gate asymmetry: with
/// both `worker_proposals` and `run_done_proposals_seam` on, a primary
/// (non-revision) execution with an armed staged PR URL but NO `run_done`
/// declaration must be held by `recheck_for_pr` exactly like `on_stop_inner`
/// holds it — never finalized purely on the staged URL's say-so.
#[tokio::test]
async fn recheck_for_pr_defers_an_undeclared_primary_staged_url_instead_of_finalizing() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);

    let staged_pr_urls = std::sync::Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());
    staged_pr_urls.record_if_unset(&execution_id, PR_URL);

    let flags_dir = tempdir().unwrap();
    let flags = empty_flags(flags_dir.path());
    flags.set("worker_proposals", true).unwrap();
    flags.set("run_done_proposals_seam", true).unwrap();

    let TestHarness { handler, .. } = TestHarness::new(db.clone(), detector.clone());
    let handler = handler
        .with_staged_pr_urls(staged_pr_urls.clone())
        .with_branch_verifier(StubBranchVerifier::ok(&expected_branch_name(
            &execution_id,
            &BranchNaming::BossExecPrefix,
            None,
        )))
        .with_feature_flags(flags);

    let outcome = handler.recheck_for_pr(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::AwaitingRunDoneDeclaration { .. }),
        "an undeclared primary staged URL must be held pending declaration, not finalized; got {outcome:?}",
    );
    assert!(
        detector.call_count() == 0,
        "the staged URL is still recovery evidence — this gate must not fall through to the cold detector",
    );

    let execution = db.get_execution(&execution_id).unwrap();
    assert!(
        execution.status.is_live(),
        "an undeclared run must never be reaped by the poller's staged-URL arm"
    );
    assert_eq!(
        task_status(&db, &chore_id),
        TaskStatus::Active,
        "the task must not advance while the declaration is still missing"
    );
}
