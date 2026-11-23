//! A CI-fix revision whose spawning attempt ended before CI went green must
//! still be closed once CI is green on the PR head — see
//! [`retire_moot_ci_fix_revisions`].

use super::helpers::*;

const PARKED: &str = "spawning_attempt_retired_unsuccessfully";

/// Detect a CI failure on `head`, returning `(attempt_id, revision_id)` of
/// the engine-created CI-fix revision.
async fn spawn_ci_fix(db: &WorkDb, product: &str, chore: &str, pr: &str, head: &str) -> (String, String) {
    let pub_ = Arc::new(RecordingPublisher::default());
    assert!(
        on_ci_failure_detected(
            db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(product, chore, pr),
            &probe(pr, head),
            &one_failure(),
        )
        .await
    );
    let attempt = db.active_ci_remediation_for_work_item(chore).unwrap().unwrap();
    (attempt.id, attempt.revision_task_id.unwrap())
}

/// What the revision reconciler does once the spawning attempt has ended
/// unsuccessfully: park the revision as `blocked`.
fn park_blocked(db: &WorkDb, revision: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = ?2 WHERE id = ?1",
            rusqlite::params![revision, PARKED],
        )
        .unwrap();
}

fn assert_closed(db: &WorkDb, revision: &str) {
    assert_eq!(task(db, revision).status, TaskStatus::Archived);
}

#[tokio::test]
async fn retriggered_attempt_then_green_closes_blocked_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/700";
    let (product, chore) = make_in_review(&db, "C-stale-retriggered", pr);
    let (attempt, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;

    // The CI worker judged the failure flaky and pushed nothing.
    db.mark_ci_remediation_retriggered(&attempt).unwrap().unwrap();
    park_blocked(&db, &revision);
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());

    assert_eq!(
        retire_moot_ci_fix_revisions(&db, &candidate(&product, &chore, pr), &[]),
        1
    );
    assert_closed(&db, &revision);
    assert!(
        !db.has_in_flight_ci_fix_revision(&chore).unwrap(),
        "the closed revision must stop counting as in flight",
    );
}

#[tokio::test]
async fn failed_attempt_then_green_closes_blocked_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/701";
    let (product, chore) = make_in_review(&db, "C-stale-failed", pr);
    let (attempt, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;

    db.mark_ci_remediation_failed(&attempt, "worker_gave_up")
        .unwrap()
        .unwrap();
    park_blocked(&db, &revision);

    assert_eq!(
        retire_moot_ci_fix_revisions(&db, &candidate(&product, &chore, pr), &[]),
        1
    );
    assert_closed(&db, &revision);
}

#[tokio::test]
async fn head_moved_then_green_closes_revision_of_the_old_head() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/702";
    let (product, chore) = make_in_review(&db, "C-stale-head-moved", pr);
    let (attempt, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;
    db.mark_ci_remediation_failed(&attempt, "worker_gave_up")
        .unwrap()
        .unwrap();
    park_blocked(&db, &revision);

    // The PR head moves to `head-2`, which is green; no attempt is active for
    // either head. The merge poller runs this sweep on every green probe
    // (see `merge_poller::tests::stale_revision_sweep_tests`).
    assert_eq!(
        retire_moot_ci_fix_revisions(&db, &candidate(&product, &chore, pr), &[]),
        1
    );
    assert_closed(&db, &revision);
}

#[tokio::test]
async fn sweep_leaves_revision_alone_while_ci_is_not_green() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/703";
    let (product, chore) = make_in_review(&db, "C-stale-inflight", pr);
    let (attempt, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;
    db.mark_ci_remediation_failed(&attempt, "worker_gave_up")
        .unwrap()
        .unwrap();
    park_blocked(&db, &revision);

    let pub_ = RecordingPublisher::default();
    let mut outcome = crate::SweepOutcome::default();
    crate::dispatch_ci_axis(
        &db,
        &pub_,
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &OpenPrCiStatus::InFlight,
        &mut outcome,
    )
    .await;

    assert_eq!(task(&db, &revision).status, TaskStatus::Blocked);
}

#[tokio::test]
async fn revision_of_a_still_active_attempt_is_not_closed() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/704";
    let (product, chore) = make_in_review(&db, "C-stale-active", pr);
    let (_attempt, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;

    assert_eq!(
        retire_moot_ci_fix_revisions(&db, &candidate(&product, &chore, pr), &[]),
        0
    );
    assert_eq!(task(&db, &revision).status, TaskStatus::Todo);
}

#[tokio::test]
async fn human_filed_revision_is_untouched() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/705";
    let (product, chore) = make_in_review(&db, "C-stale-human", pr);
    let (attempt, ci_revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;
    db.mark_ci_remediation_failed(&attempt, "worker_gave_up")
        .unwrap()
        .unwrap();
    park_blocked(&db, &ci_revision);

    // A human-filed revision, and one filed for a review finding, on the
    // same parent. Even if the human one is (wrongly) linked from an ended
    // attempt row, the `ci-fix:` provenance check must keep it out.
    let human = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(chore.clone())
                .description("Please rename the helper")
                .force_duplicate(true)
                .build(),
            &fix_checker(),
        )
        .unwrap();
    let review = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(chore.clone())
                .description("Address review findings")
                .created_via(format!("{}exec_review", boss_protocol::CREATED_VIA_PR_REVIEW_PREFIX))
                .force_duplicate(true)
                .build(),
            &fix_checker(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE ci_remediations SET revision_task_id = ?2 WHERE id = ?1",
            rusqlite::params![attempt, human.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked' WHERE id IN (?1, ?2)",
            rusqlite::params![human.id, review.id],
        )
        .unwrap();

    assert_eq!(
        retire_moot_ci_fix_revisions(&db, &candidate(&product, &chore, pr), &[]),
        0
    );
    assert_eq!(task(&db, &human.id).status, TaskStatus::Blocked);
    assert_eq!(task(&db, &review.id).status, TaskStatus::Blocked);
}

/// A queue-side attempt that has ended (`abandoned`) strands its CI-fix
/// revision; a green PR-head probe closes it so the parent is not held in
/// Review ▸ Blocked.
#[tokio::test]
async fn queue_side_abandoned_attempt_revision_is_closed_when_head_is_green() {
    for (label, failure_kind) in [
        ("C-stale-queue-side-trunk", "trunk_queue_eviction"),
        ("C-stale-queue-side-rebounce", "merge_queue_rebounce"),
    ] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/706";
        let (product, chore) = make_in_review(&db, label, pr);
        let (attempt, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;
        db.mark_ci_remediation_abandoned(&attempt, "queue_side_attempt_ended")
            .unwrap()
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE ci_remediations SET failure_kind = ?2 WHERE id = ?1",
                rusqlite::params![attempt, failure_kind],
            )
            .unwrap();
        park_blocked(&db, &revision);

        assert_eq!(
            retire_moot_ci_fix_revisions(&db, &candidate(&product, &chore, pr), &[]),
            1,
            "{failure_kind} revision must close once the attempt has ended and the PR head is green"
        );
        assert_closed(&db, &revision);
    }
}

// ── An attempt that is still active but can no longer run ────────────────────
//
// `retire_moot_ci_fix_revisions` above needs the attempt to have ended. These
// cover `retire_moot_pending_ci_fixes`: the attempt is still `pending`, no
// worker is on its revision, and nothing else would ever end it.

fn executions_to(db: &WorkDb, revision: &str, status: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = ?2 WHERE work_item_id = ?1",
            rusqlite::params![revision, status],
        )
        .unwrap();
}

fn attempt(db: &WorkDb, id: &str) -> crate::work::CiRemediation {
    db.list_ci_remediations(None, &[], None, None)
        .unwrap()
        .into_iter()
        .find(|attempt| attempt.id == id)
        .expect("attempt row")
}

#[tokio::test]
async fn a_pending_head_fix_no_worker_started_is_retired_once_the_head_is_green() {
    // Whatever the mergeability: the conflict arm of the sweep runs no other
    // CI retire path, so a green-and-conflicting PR is the case that matters.
    for conflicting in [false, true] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/710";
        let (product, chore) = make_in_review(&db, "C-moot-head-fix", pr);
        let (attempt_id, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;
        // Its worker was cancelled before it did anything.
        executions_to(&db, &revision, "cancelled");

        let pub_ = RecordingPublisher::default();
        assert_eq!(
            retire_moot_pending_ci_fixes(&db, &pub_, &candidate(&product, &chore, pr), conflicting).await,
            1,
            "conflicting={conflicting}"
        );
        assert_closed(&db, &revision);
        let ended = attempt(&db, &attempt_id);
        assert_eq!(ended.status, "abandoned");
        assert_eq!(ended.failure_reason.as_deref(), Some("moot_head_ci_green"));
        assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
        assert!(!db.has_in_flight_ci_fix_revision(&chore).unwrap());
        assert_eq!(
            pub_.ci_failure_cleared_count(pr).await,
            1,
            "the badge is driven by active attempts and needs the explicit clear"
        );
        // Nothing left to do on the next pass.
        assert_eq!(
            retire_moot_pending_ci_fixes(&db, &pub_, &candidate(&product, &chore, pr), conflicting).await,
            0
        );
    }
}

#[tokio::test]
async fn a_fix_a_worker_is_on_is_never_retired_from_under_it() {
    for status in ["claimed", "running", "waiting_human"] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/711";
        let (product, chore) = make_in_review(&db, "C-moot-running", pr);
        let (attempt_id, revision) = spawn_ci_fix(&db, &product, &chore, pr, "head-1").await;
        executions_to(&db, &revision, status);

        let pub_ = RecordingPublisher::default();
        assert_eq!(
            retire_moot_pending_ci_fixes(&db, &pub_, &candidate(&product, &chore, pr), true).await,
            0,
            "{status}"
        );
        assert_ne!(task(&db, &revision).status, TaskStatus::Archived, "{status}");
        assert_eq!(attempt(&db, &attempt_id).status, "pending", "{status}");
        assert_eq!(pub_.ci_failure_cleared_count(pr).await, 0, "{status}");
    }
}

/// The whole pass, for the state that left a pull request stuck for days. A
/// merge-queue build failed and a CI-fix revision was created for it; the fix
/// worker was cancelled and the revision left unstarted; the PR then
/// conflicted with its base, and the work to resolve the conflict queued
/// behind the dead revision. Every check on the PR head passed throughout.
#[tokio::test]
async fn sweep_retires_a_cancelled_queue_fix_once_the_green_pr_conflicts() {
    use crate::tests::StubProbe;

    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/712";
    let (product, chore) = make_in_review(&db, "C-queue-fix-cancelled", pr);
    let publisher = Arc::new(RecordingPublisher::default());
    assert!(
        on_merge_queue_rebounce_detected(
            &db,
            crate::queue_ci_contract::test_context(publisher.as_ref()),
            &candidate(&product, &chore, pr),
            "head-1",
            "queue-commit-1",
            None,
            &one_failure(),
        )
        .await
    );
    let queue_attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("queue attempt");
    assert_eq!(queue_attempt.failure_kind.as_deref(), Some("merge_queue_rebounce"));
    let revision = queue_attempt.revision_task_id.clone().expect("queue fix revision");
    executions_to(&db, &revision, "cancelled");
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET autostart = 0 WHERE id = ?1", [&revision])
        .unwrap();

    let probe = StubProbe::new();
    let green = |mergeability| {
        PrLifecycleState::Open(OpenPrStatus {
            mergeability,
            ci: OpenPrCiStatus::Clean,
        })
    };

    // Green and mergeable, with nothing waiting on the stopped fix: head CI
    // says nothing about a merge-queue failure, so nothing moves and the
    // stored CI state is still the queue's failure.
    probe.set_with_base_head(pr, green(OpenPrMergeability::Clean), "base-1", "head-1");
    crate::run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(attempt(&db, &queue_attempt.id).status, "pending");
    assert_eq!(task(&db, &revision).status, TaskStatus::Todo);
    assert_eq!(task(&db, &chore).ci_required_state.as_deref(), Some("fail"));

    // The next writer on the PR waits behind the dead revision.
    let conflict_work = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(chore.clone())
                .description("Resolve the conflicts with the base branch")
                .force_duplicate(true)
                .build(),
            &fix_checker(),
        )
        .unwrap();
    assert_eq!(
        db.gating_prereqs_for(&conflict_work.id).unwrap(),
        vec![revision.clone()],
        "precondition: the conflict work is queued behind the dead CI-fix revision"
    );

    // Green and conflicting: the fix cannot matter at this head any more.
    probe.set_with_base_head(pr, green(OpenPrMergeability::Conflict), "base-1", "head-1");
    crate::run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    let ended = attempt(&db, &queue_attempt.id);
    assert_eq!(ended.status, "abandoned");
    assert_eq!(
        ended.failure_reason.as_deref(),
        Some("moot_conflict_preempts_queue_fix")
    );
    assert_closed(&db, &revision);
    assert!(
        db.gating_prereqs_for(&conflict_work.id).unwrap().is_empty(),
        "the conflict work must no longer wait on the closed revision"
    );
    assert_ne!(task(&db, &conflict_work.id).status, TaskStatus::Blocked);
    // The same pass writes the head's real CI state and clears the badge.
    assert_eq!(task(&db, &chore).ci_required_state.as_deref(), Some("success"));
    assert!(publisher.ci_failure_cleared_count(pr).await >= 1);
}
