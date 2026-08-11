//! Regression coverage for the dispatch pause's admission contract.
//!
//! The pause is admission-only: it must stop every in-scope work item short
//! of a worker spawn and a cube lease, without touching anything already
//! running, and the queue it accumulates must drain on resume.
//!
//! [`super::review_pause`] already covers the ready-queue drain's own gate.
//! This module covers the part the 2026-08-10 incident actually broke — the
//! paths that reach a spawn *without* going through that drain — and the
//! chokepoint gate that now backstops all of them:
//!
//! - every enqueueing entry point (fresh autostart row, dependency clearing,
//!   the recovery/retry sweeps' `request_execution`, the dead-review
//!   re-enqueue) queues but does not spawn, and drains on resume;
//! - `schedule_execution` refuses a queued row even when the pause lands
//!   after its slot was claimed — the case the drain's own gate cannot see;
//! - the breaker's recovery canary never spends a `pr_review` row, which is
//!   what makes `reviews: held` true under a breaker pause;
//! - an explicit operator force-dispatch still overrides the pause, which is
//!   deliberate and must not regress.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;

use boss_protocol::{ExecutionKind, PauseReason};

use crate::dispatch_events::RecordingDispatchEventSink;
use crate::spawn_health::{SpawnHealthTracker, maybe_admit_recovery_probe};

/// Settle time for "assert nothing happened". There is no positive event to
/// wait on when the correct behaviour is that no worker spawns, so the test
/// gives an incorrect dispatch a generous window to appear in.
const NOTHING_HAPPENS_WINDOW: Duration = Duration::from_millis(150);

/// A coordinator with a review pool, wired to recording fakes so a test can
/// assert on all three things a pause must prevent — a worker running, a
/// cube workspace being leased, and the refusal going unreported.
fn paused_dispatch_fixture(
    db: Arc<WorkDb>,
) -> (
    Arc<ExecutionCoordinator>,
    Arc<FakeCubeClient>,
    Arc<FakeExecutionRunner>,
    Arc<RecordingDispatchEventSink>,
) {
    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let events = Arc::new(RecordingDispatchEventSink::new());
    // Pools sized above the number of rows any test here queues: the runner
    // is `pending`, so a dispatched worker never releases its slot, and a
    // test asserting "everything drained on resume" must not be able to fail
    // for want of a slot instead.
    let mut coord = ExecutionCoordinator::new(db, WorkerPool::new(6), cube.clone(), runner.clone());
    coord.set_review_pool(WorkerPool::new_review(2));
    coord.set_automation_pool(WorkerPool::new_automation(2));
    let coord = coord.with_dispatch_events(events.clone());
    (Arc::new(coord), cube, runner, events)
}

fn breaker_pause(coordinator: &ExecutionCoordinator) {
    coordinator.pause_dispatch(
        1_786_385_816,
        DispatchPauseOrigin::Breaker,
        PauseReason::new("test: spawn-capability breaker tripped").unwrap(),
    );
}

fn operator_pause(coordinator: &ExecutionCoordinator) {
    coordinator.pause_dispatch(
        1_786_385_816,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: operator pause").unwrap(),
    );
}

fn ready_pr_review_execution(db: &WorkDb, work_item_id: &str) -> WorkExecution {
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(work_item_id.to_owned())
            .kind(ExecutionKind::PrReview)
            .status(ExecutionStatus::Ready)
            .build(),
    )
    .unwrap()
}

// ── Every enqueueing entry point queues, none spawns ──────────────────────

/// **The headline regression.** Pause with the breaker, then drive every
/// production path that can put a work item into the ready queue — a fresh
/// autostart row, a dependency clearing, the plain `request_execution` the
/// recovery/retry sweeps use, and a `pr_review` re-enqueue of the shape
/// `pr_review_recovery` produces — kicking the scheduler after each, exactly
/// as those callers do.
///
/// Nothing may spawn and nothing may lease a cube workspace for as long as
/// the pause holds, and every row must then drain on resume. Enqueueing
/// itself is correct: the pause is admission-only, so held work accumulates
/// rather than being dropped.
#[tokio::test]
async fn breaker_pause_holds_every_enqueue_entry_point_then_drains_on_resume() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let (coordinator, cube, runner, _events) = paused_dispatch_fixture(db.clone());
    breaker_pause(coordinator.as_ref());

    let mut expected_ready: Vec<String> = Vec::new();

    // 1. Auto-dispatch of a freshly filed autostart row — the reconcile pass
    //    that turns a new chore into a `ready` execution.
    let fresh = create_test_chore(&db, product.id.clone(), "Filed during the pause");
    db.reconcile_product_executions(&product.id).unwrap();
    expected_ready.push(db.list_executions(Some(&fresh.id)).unwrap()[0].id.clone());
    coordinator.kick();

    // 2. A dependency gate clearing. The prereq is completed while paused, so
    //    the dependent becomes eligible and gets requested.
    let prereq = create_test_chore(&db, product.id.clone(), "Prereq");
    let dependent = create_test_chore(&db, product.id.clone(), "Blocked on the prereq");
    db.add_dependency(AddDependencyInput {
        dependent: dependent.id.clone(),
        prerequisite: prereq.id.clone(),
        relation: None,
    })
    .unwrap();
    db.update_work_item(
        &prereq.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let unblocked = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(dependent.id.clone())
                .build(),
        )
        .unwrap();
    expected_ready.push(unblocked.id.clone());
    coordinator.kick();

    // 3. The retry/recovery sweep shape: a plain `request_execution` against
    //    an existing work item (orphan_sweep, dispatch_failure_recovery_sweep,
    //    pool_claim_sweep, host_reconcile and the rest all do exactly this,
    //    then kick).
    let orphaned = create_test_chore(&db, product.id.clone(), "Re-enqueued by a sweep");
    let resweep = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(orphaned.id.clone())
                .build(),
        )
        .unwrap();
    expected_ready.push(resweep.id.clone());
    coordinator.kick();

    // 4. The dead-review recovery shape — a fresh `pr_review` row, which under
    //    a *breaker* pause is held like everything else.
    let (_, reviewed_chore) = make_pr_review_fixture(&db, Some("https://github.com/spinyfin/mono/pull/2712"));
    let review = ready_pr_review_execution(&db, &reviewed_chore);
    expected_ready.push(review.id.clone());
    coordinator.kick();

    sleep(NOTHING_HAPPENS_WINDOW).await;

    assert_eq!(
        runner.calls.lock().await.len(),
        0,
        "no worker may spawn while dispatch is breaker-paused, from any entry point"
    );
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "no cube workspace may be leased while dispatch is breaker-paused"
    );
    for execution_id in &expected_ready {
        assert_eq!(
            db.get_execution(execution_id).unwrap().status,
            ExecutionStatus::Ready,
            "execution {execution_id} must still be queued, not dispatched, while paused"
        );
    }

    // Resume drains the whole backlog — the queue the pause accumulated is
    // work deferred, not work dropped.
    coordinator.resume_dispatch();
    coordinator.kick();
    for execution_id in &expected_ready {
        wait_for_execution_status(db.as_ref(), execution_id, ExecutionStatus::Running).await;
    }
    // Give any (incorrect) duplicate dispatch a window to land before
    // asserting the count — the same settle the resume-drain test in
    // `review_pause` uses.
    sleep(NOTHING_HAPPENS_WINDOW).await;
    assert_eq!(
        runner.calls.lock().await.len(),
        expected_ready.len(),
        "every held row must dispatch exactly once on resume"
    );
}

// ── The chokepoint backstop ───────────────────────────────────────────────

/// The drain checks the pause before claiming a slot, but hands the slow
/// tail (`cube repo ensure`, the lease, the spawn) to a detached task — so a
/// pause can land in between. `schedule_execution` is the chokepoint every
/// spawn passes through, and must refuse there too, before any cube lease.
///
/// This is also what makes the gate un-forgettable: a dispatch entry point
/// added tomorrow reaches this same check whether or not its author thought
/// about the pause.
#[tokio::test]
async fn schedule_execution_refuses_a_queued_row_when_a_pause_lands_after_the_claim() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Racing the pause");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&chore.id)).unwrap()[0].clone();

    let (coordinator, cube, runner, events) = paused_dispatch_fixture(db.clone());

    // The slot is claimed first — i.e. the drain already passed its own gate.
    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("main pool slot available");

    // ...and only now does the breaker trip.
    breaker_pause(coordinator.as_ref());

    let result = coordinator
        .schedule_execution(&execution, &worker_id, DispatchAdmission::Queued)
        .await;
    assert!(
        result.is_err(),
        "schedule_execution must refuse a queued row once dispatch is paused"
    );
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "a held dispatch must be refused before any cube workspace is leased"
    );
    assert_eq!(
        runner.calls.lock().await.len(),
        0,
        "a held dispatch must not reach the runner"
    );

    let recorded = events.events().await;
    let held = recorded
        .iter()
        .find(|e| e.stage == "dispatch_held_by_pause")
        .expect("the refusal must be visible in `bossctl dispatch tail`");
    assert_eq!(
        held.outcome, "skipped",
        "a held dispatch is the pause working, not a failure"
    );
    assert_eq!(held.details["origin"], serde_json::json!("breaker"));
    assert_eq!(held.details["admission"], serde_json::json!("queued"));
    assert_eq!(held.details["reviews_held"], serde_json::json!(true));
}

/// The chokepoint gate must not over-reach: an operator pause exempts
/// `pr_review` rows, and that exemption has to hold at
/// `schedule_execution` exactly as it does in the drain. Without this, the
/// fix for the breaker case would silently break reviews under the operator
/// pause the exemption was designed for.
#[tokio::test]
async fn operator_pause_still_admits_a_review_at_the_chokepoint() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (_, chore_id) = make_pr_review_fixture(&db, None);
    let execution = ready_pr_review_execution(&db, &chore_id);

    let (coordinator, cube, _runner, _events) = paused_dispatch_fixture(db.clone());
    operator_pause(coordinator.as_ref());

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("review pool slot available");

    let result = coordinator
        .schedule_execution(&execution, &worker_id, DispatchAdmission::Queued)
        .await;
    assert!(
        result.is_ok(),
        "an operator pause exempts reviews; the chokepoint must honour that too: {result:?}"
    );
    // A leased workspace is the positive counterpart to the held case's "no
    // lease was taken": the review really was admitted, not merely
    // not-refused.
    assert_eq!(
        cube.lease_calls.lock().await.len(),
        1,
        "the exempt review must have leased a workspace and dispatched"
    );
}

// ── The breaker's recovery canary ─────────────────────────────────────────

/// The canary must come from the non-review work, even when a `pr_review`
/// row is the last (least urgent) thing in the queue — which is exactly what
/// the incident produced, because a review re-enqueued by
/// `pr_review_dead_recovery` sorts last and the probe took the last row.
///
/// Spending a review as the canary is what fed the review churn guard until
/// items parked with unreviewed open PRs, and it is what made
/// `reviews: held` a false statement.
#[tokio::test]
async fn recovery_probe_never_spends_a_pr_review_row() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "ordinary work");
    let db = Arc::new(db);
    let chore_execution = create_ready_chore_execution(&db, &work_item_id);

    // A `pr_review` row queued *after* the chore, so it sorts last and would
    // be the probe's pick under the old unfiltered `.last()`.
    let (_, reviewed_chore) = make_pr_review_fixture(&db, Some("https://github.com/spinyfin/mono/pull/2712"));
    let review_execution = ready_pr_review_execution(&db, &reviewed_chore);
    assert_eq!(
        db.list_ready_executions().unwrap().last().map(|e| e.id.clone()),
        Some(review_execution.id.clone()),
        "fixture precondition: the review must sort last in the ready queue"
    );

    let coordinator = make_dispatchable_coordinator(db.clone(), 4);
    breaker_pause(coordinator.as_ref());

    let spawn_health = SpawnHealthTracker::new();
    let events = Arc::new(RecordingDispatchEventSink::default());
    maybe_admit_recovery_probe(&db, &coordinator, &spawn_health, events.as_ref(), 1000).await;

    assert!(
        spawn_health.is_probe_execution(&chore_execution.id),
        "the canary must be the non-review row"
    );
    assert!(
        !spawn_health.is_probe_execution(&review_execution.id),
        "a `pr_review` execution must never be admitted as a recovery canary"
    );
    assert_eq!(
        db.get_execution(&review_execution.id).unwrap().status,
        ExecutionStatus::Ready,
        "the review must still be queued — a breaker pause holds reviews"
    );

    let recorded = events.events().await;
    let admitted = recorded
        .iter()
        .find(|e| e.stage == "breaker_recovery_probe_admitted")
        .expect("the canary's pause bypass must be declared in the dispatch stream");
    assert_eq!(admitted.execution_id, chore_execution.id);
    assert_eq!(admitted.details["skipped_reviews"], serde_json::json!(1));
}

/// When reviews are the *only* ready work, the probe admits nothing rather
/// than falling back to one. The breaker still recovers via a fresh app
/// session or `bossctl dispatch resume`; what it must not do is buy its
/// recovery evidence with a PR's only review.
#[tokio::test]
async fn recovery_probe_admits_nothing_when_only_reviews_are_ready() {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let (_, reviewed_chore) = make_pr_review_fixture(&db, Some("https://github.com/spinyfin/mono/pull/2713"));
    let review_execution = ready_pr_review_execution(&db, &reviewed_chore);

    let coordinator = make_dispatchable_coordinator(db.clone(), 4);
    breaker_pause(coordinator.as_ref());

    let spawn_health = SpawnHealthTracker::new();
    let events = Arc::new(RecordingDispatchEventSink::default());
    maybe_admit_recovery_probe(&db, &coordinator, &spawn_health, events.as_ref(), 1000).await;

    assert!(
        !spawn_health.is_probe_execution(&review_execution.id),
        "no canary may be admitted when every ready row is a PR review"
    );
    assert_eq!(
        db.get_execution(&review_execution.id).unwrap().status,
        ExecutionStatus::Ready,
        "the review must remain queued"
    );
    assert!(
        events.events().await.is_empty(),
        "nothing was admitted, so nothing may be reported as admitted"
    );
}

/// An operator pause is manual-resume-only: the breaker's canary must never
/// be admitted through one, whatever the tracker's backoff says.
#[tokio::test]
async fn recovery_probe_never_runs_under_an_operator_pause() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "ordinary work");
    let db = Arc::new(db);
    let execution = create_ready_chore_execution(&db, &work_item_id);

    let coordinator = make_dispatchable_coordinator(db.clone(), 4);
    operator_pause(coordinator.as_ref());

    let spawn_health = SpawnHealthTracker::new();
    let events = Arc::new(RecordingDispatchEventSink::default());
    maybe_admit_recovery_probe(&db, &coordinator, &spawn_health, events.as_ref(), 1000).await;

    assert!(
        !spawn_health.is_probe_execution(&execution.id),
        "an operator pause must never be auto-probed"
    );
    assert_eq!(db.get_execution(&execution.id).unwrap().status, ExecutionStatus::Ready);
}

// ── The explicit operator override ────────────────────────────────────────

/// `bossctl agents launch` force-dispatches one named execution straight
/// through a pause — including a breaker pause, which an operator may be
/// probing deliberately. That is a decided feature, not a bypass to close;
/// this test pins it so tightening the pause elsewhere cannot regress it.
#[tokio::test]
async fn operator_forced_dispatch_overrides_a_breaker_pause() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "launched by hand");
    let db = Arc::new(db);
    let execution = create_ready_chore_execution(&db, &work_item_id);

    let coordinator = make_dispatchable_coordinator(db.clone(), 4);
    breaker_pause(coordinator.as_ref());

    let worker_id = coordinator
        .force_dispatch(&execution.id, DispatchAdmission::OperatorForced)
        .await
        .expect("an explicit operator launch must override the pause");
    assert!(worker_id.starts_with("worker-"), "got {worker_id:?}");
    assert_ne!(
        db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Ready,
        "the force-dispatched execution must have left the ready queue"
    );
}

/// The same call without operator intent is refused. `force_dispatch` is not
/// a blanket pause bypass — the admission decides, which is what stopped it
/// from being the hole it used to be.
#[tokio::test]
async fn force_dispatch_without_operator_intent_is_still_held() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "not launched by hand");
    let db = Arc::new(db);
    let execution = create_ready_chore_execution(&db, &work_item_id);

    let coordinator = make_dispatchable_coordinator(db.clone(), 4);
    operator_pause(coordinator.as_ref());

    let result = coordinator
        .force_dispatch(&execution.id, DispatchAdmission::BreakerRecoveryProbe)
        .await;
    assert!(
        result.is_err(),
        "a recovery canary must not be admitted through an operator pause"
    );
    assert_eq!(
        db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Ready,
        "the refused row must stay queued"
    );
}

// ── PauseBypassOverride at the chokepoint ─────────────────────────────────
//
// `#2705` wires `bossctl work start --force` through
// `dispatch_with_pause_bypass` and the ready-queue drain's marker set; this
// PR threads that path through `schedule_execution` as
// `DispatchAdmission::PauseBypassOverride`. The request-layer tests refuse
// a breaker pause *before* the drain, so they never exercise the
// `dispatch_hold_for` arm. These two pin the chokepoint itself.

/// `PauseBypassOverride` is admitted at the chokepoint under an
/// operator-origin pause — the case `bossctl work start --force` is for.
#[tokio::test]
async fn pause_bypass_override_is_admitted_under_an_operator_pause() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "operator forced while paused");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&chore.id)).unwrap()[0].clone();

    let (coordinator, cube, _runner, _events) = paused_dispatch_fixture(db.clone());
    operator_pause(coordinator.as_ref());

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("main pool slot available");

    let result = coordinator
        .schedule_execution(&execution, &worker_id, DispatchAdmission::PauseBypassOverride)
        .await;
    assert!(
        result.is_ok(),
        "PauseBypassOverride must be admitted under an operator pause: {result:?}"
    );
    assert_eq!(
        cube.lease_calls.lock().await.len(),
        1,
        "the pause-bypass override must lease a workspace and dispatch"
    );
}

/// Under a breaker pause the same admission is held: the pause-only force
/// does not grow past an operator-origin pause, and the refusal is labelled
/// so `dispatch tail` shows which path asked.
#[tokio::test]
async fn pause_bypass_override_is_held_under_a_breaker_pause() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "not forceable through breaker");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&chore.id)).unwrap()[0].clone();

    let (coordinator, cube, runner, events) = paused_dispatch_fixture(db.clone());

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("main pool slot available");

    breaker_pause(coordinator.as_ref());

    let result = coordinator
        .schedule_execution(&execution, &worker_id, DispatchAdmission::PauseBypassOverride)
        .await;
    assert!(
        result.is_err(),
        "PauseBypassOverride must be held under a breaker pause"
    );
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "a held pause-bypass override must not lease a cube workspace"
    );
    assert_eq!(
        runner.calls.lock().await.len(),
        0,
        "a held pause-bypass override must not reach the runner"
    );

    let recorded = events.events().await;
    let held = recorded
        .iter()
        .find(|e| e.stage == "dispatch_held_by_pause")
        .expect("the refusal must be visible in `bossctl dispatch tail`");
    assert_eq!(held.outcome, "skipped");
    assert_eq!(held.details["origin"], serde_json::json!("breaker"));
    assert_eq!(held.details["admission"], serde_json::json!("pause_bypass_override"));
    assert_eq!(held.details["reviews_held"], serde_json::json!(true));
}

// ── Pause state is one value ──────────────────────────────────────────────

/// Resuming clears the whole episode. Previously `reviews_exempt` was never
/// cleared, so after resuming an operator pause `GetDispatchState` kept
/// reporting a scope for a pause that no longer existed — and the next
/// reader could not tell a live scope from a leftover one.
#[tokio::test]
async fn resume_clears_the_entire_pause_episode() {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let coordinator = make_dispatchable_coordinator(db, 1);

    operator_pause(coordinator.as_ref());
    let pause = coordinator.dispatch_pause().expect("paused");
    assert_eq!(pause.origin, DispatchPauseOrigin::Operator);
    assert!(!pause.reviews_held(), "an operator pause exempts reviews");
    assert!(coordinator.dispatch_pause_exempts_reviews());

    coordinator.resume_dispatch();
    assert!(coordinator.dispatch_pause().is_none());
    assert!(!coordinator.is_dispatch_paused());
    assert!(
        !coordinator.dispatch_pause_exempts_reviews(),
        "a resumed engine exempts nothing, because it holds nothing"
    );
    assert_eq!(coordinator.dispatch_paused_reason(), None);
    assert_eq!(coordinator.dispatch_paused_since_epoch_s(), None);

    // A subsequent breaker pause reports its own scope, not the previous
    // episode's.
    breaker_pause(coordinator.as_ref());
    assert!(
        coordinator.dispatch_pause().expect("paused").reviews_held(),
        "a breaker pause holds reviews regardless of what the last pause exempted"
    );
    assert!(!coordinator.dispatch_pause_exempts_reviews());
}

/// A running engine holds nothing, so `dispatch_hold_for` admits every
/// admission — including the ordinary queued one. Guards against the gate
/// accidentally becoming fail-closed.
#[tokio::test]
async fn nothing_is_held_when_dispatch_is_not_paused() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "ordinary work");
    let db = Arc::new(db);
    let execution = create_ready_chore_execution(&db, &work_item_id);
    let coordinator = make_dispatchable_coordinator(db, 1);

    for admission in [
        DispatchAdmission::Queued,
        DispatchAdmission::OperatorForced,
        DispatchAdmission::BreakerRecoveryProbe,
    ] {
        assert!(
            coordinator.dispatch_hold_for(&execution, admission).is_none(),
            "{admission:?} must not be held while dispatch is running"
        );
    }
}
