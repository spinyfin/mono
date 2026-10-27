//! The `all_spawns` scope holds dispatch across every pool, including PR reviews
//! and review guides, except explicit `bossctl agents launch` overrides.
//! The default operator pause keeps reviews exempt (see [`super::review_pause`]).
//! This module pins what the all_spawns scope guarantees:
//!
//! - the hold is at admission, in both the ready-queue drain and the
//!   `schedule_execution` chokepoint ([`ExecutionCoordinator::dispatch_hold_for`]),
//!   for every review-pool kind, with no list of review kinds anywhere;
//! - origin stays `operator`, so the breaker's recovery probe is never admitted
//!   and the breaker's own trip cannot replace the operator's hold;
//! - the per-request pause override does not apply to it.

use super::helpers::*;

use boss_protocol::{DispatchPauseScope, ExecutionKind, PauseReason};

use crate::dispatch_events::RecordingDispatchEventSink;
use crate::spawn_health::{SpawnHealthTracker, TripSignal, maybe_admit_recovery_probe, trip_spawn_capability_circuit};

fn operator_pause(coordinator: &ExecutionCoordinator, scope: DispatchPauseScope) {
    coordinator.pause_dispatch_scoped(
        1_786_385_816,
        DispatchPauseOrigin::Operator,
        scope,
        PauseReason::new("test: operator pause").unwrap(),
    );
}

fn ready_review_pool_execution(db: &WorkDb, work_item_id: &str, kind: ExecutionKind) -> WorkExecution {
    seed_local_claude_driver(db);
    if kind == ExecutionKind::PrReviewGuide {
        crate::test_support::insert_host_capability(db, "local", "driver=codex", "auto");
        let (_, comparison) = seed_review_guide_series(db, work_item_id);
        return db
            .create_pr_review_guide_execution(&comparison, "https://github.com/test/repo")
            .unwrap();
    }
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(work_item_id.to_owned())
            .kind(kind)
            .status(ExecutionStatus::Ready)
            .build(),
    )
    .unwrap()
}

const REVIEW_POOL_KINDS: [ExecutionKind; 2] = [ExecutionKind::PrReview, ExecutionKind::PrReviewGuide];

#[tokio::test]
async fn critical_admission_respects_operator_all_spawns_scope() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let work_item = create_active_chore(&db, &product, "Urgent repair");
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET priority = 'critical' WHERE id = ?1", [&work_item])
        .unwrap();
    let db = Arc::new(db);
    let execution = create_ready_chore_execution(&db, &work_item);
    let coordinator = make_dispatchable_coordinator(db.clone(), 4);

    operator_pause(&coordinator, DispatchPauseScope::NewWork);
    assert!(
        coordinator
            .dispatch_hold_for(&execution, DispatchAdmission::Critical)
            .is_none()
    );
    operator_pause(&coordinator, DispatchPauseScope::AllSpawns);
    let hold = coordinator
        .dispatch_hold_for(&execution, DispatchAdmission::Critical)
        .expect("critical priority cannot override an all-spawns hold");
    assert!(hold.detail.contains("all_spawns"));
    let admission = coordinator.evaluate_dispatch_admission(&work_item).await.unwrap();
    assert!(admission.pause.active);
    assert!(!admission.pause.overridable);
    assert_eq!(admission.pause.scope, Some(DispatchPauseScope::AllSpawns));
    coordinator.drain_ready_queue().await;
    let held = db.get_execution(&execution.id).unwrap();
    assert_eq!(held.status, ExecutionStatus::Ready);
    assert_eq!(held.dispatch_wait_reason.as_deref(), Some("dispatch_paused"));
}

/// Drain-level: an all-spawns operator pause leaves a ready review-pool
/// execution `ready` and un-spawned, for each review-pool kind.
#[tokio::test]
async fn all_spawns_operator_pause_holds_ready_review_and_review_guide_executions() {
    for kind in REVIEW_POOL_KINDS {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let (_, chore_id) = make_pr_review_fixture(&db, None);
        let execution = ready_review_pool_execution(&db, &chore_id, kind.clone());

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
        coord.set_review_pool(WorkerPool::new_review(1));
        let coordinator = Arc::new(coord);

        operator_pause(&coordinator, DispatchPauseScope::AllSpawns);
        coordinator.drain_ready_queue().await;
        assert_eq!(
            runner.calls.lock().await.len(),
            0,
            "{kind:?}: an all-spawns pause must hold review-pool executions"
        );
        assert_eq!(
            db.get_execution(&execution.id).unwrap().status,
            ExecutionStatus::Ready,
            "{kind:?}: the held execution must stay queued"
        );
        assert!(
            cube.lease_calls.lock().await.is_empty(),
            "{kind:?}: a held execution must not lease a cube workspace"
        );

        // Both review kinds drain once the operator resumes dispatch.
        coordinator.resume_dispatch();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;
    }
}

/// The default scope lets both ready review kinds through the drain.
#[tokio::test]
async fn default_operator_pause_still_lets_ready_review_pool_executions_through() {
    for kind in REVIEW_POOL_KINDS {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let (_, chore_id) = make_pr_review_fixture(&db, None);
        let execution = ready_review_pool_execution(&db, &chore_id, kind);

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
        coord.set_review_pool(WorkerPool::new_review(1));
        let coordinator = Arc::new(coord);

        operator_pause(&coordinator, DispatchPauseScope::NewWork);
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;

        assert_eq!(
            runner.calls.lock().await.len(),
            1,
            "the default operator pause must keep exempting review-pool executions"
        );
    }
}

/// Chokepoint-level: `dispatch_hold_for` — the single authority every spawn
/// passes through — refuses a queued review-pool row under all-spawns and
/// admits it under the default scope, for every review-pool kind.
#[tokio::test]
async fn dispatch_hold_for_holds_review_pool_rows_only_under_all_spawns() {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let (_, chore_id) = make_pr_review_fixture(&db, None);
    let coordinator = make_dispatchable_coordinator(db.clone(), 4);

    for kind in REVIEW_POOL_KINDS {
        let execution = ready_review_pool_execution(&db, &chore_id, kind.clone());

        operator_pause(coordinator.as_ref(), DispatchPauseScope::NewWork);
        assert!(
            coordinator
                .dispatch_hold_for(&execution, DispatchAdmission::Queued)
                .is_none(),
            "{kind:?}: exempt under the default scope"
        );

        operator_pause(coordinator.as_ref(), DispatchPauseScope::AllSpawns);
        let hold = coordinator
            .dispatch_hold_for(&execution, DispatchAdmission::Queued)
            .unwrap_or_else(|| panic!("{kind:?}: must be held under all_spawns"));
        assert!(hold.targets_review_pool);
        assert_eq!(hold.pause.origin, DispatchPauseOrigin::Operator);
        assert_eq!(hold.pause.scope, DispatchPauseScope::AllSpawns);
        assert!(hold.detail.contains("all_spawns"), "{}", hold.detail);

        coordinator.resume_dispatch();
    }
}

/// An explicit `bossctl agents launch` still overrides every pause, this one
/// included; that is a decided feature.
#[tokio::test]
async fn operator_forced_dispatch_is_never_held_by_all_spawns() {
    let (_dir, db) = open_db();
    let db = Arc::new(db);
    let (_, chore_id) = make_pr_review_fixture(&db, None);
    let execution = ready_review_pool_execution(&db, &chore_id, ExecutionKind::PrReview);
    let coordinator = make_dispatchable_coordinator(db, 4);

    operator_pause(coordinator.as_ref(), DispatchPauseScope::AllSpawns);

    assert!(
        coordinator
            .dispatch_hold_for(&execution, DispatchAdmission::OperatorForced)
            .is_none()
    );
}

/// The per-request override (`work start --force`) is for the default
/// operator pause only; an all-spawns hold refuses it, and says why.
#[tokio::test]
async fn pause_bypass_override_is_refused_under_all_spawns_but_not_the_default_scope() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "ordinary work");
    let db = Arc::new(db);
    let execution = create_ready_chore_execution(&db, &work_item_id);
    let coordinator = make_dispatchable_coordinator(db, 4);

    operator_pause(coordinator.as_ref(), DispatchPauseScope::NewWork);
    assert!(
        coordinator
            .dispatch_hold_for(&execution, DispatchAdmission::PauseBypassOverride)
            .is_none()
    );

    operator_pause(coordinator.as_ref(), DispatchPauseScope::AllSpawns);
    let hold = coordinator
        .dispatch_hold_for(&execution, DispatchAdmission::PauseBypassOverride)
        .expect("an all-spawns pause refuses the pause-only override");
    assert!(hold.detail.contains("all_spawns"), "{}", hold.detail);

    // The admission evaluator the app and `work start --force` consult agrees:
    // the pause is active and not overridable, and names its scope.
    let snapshot = coordinator.dispatch_pause().expect("paused");
    assert!(!snapshot.accepts_pause_bypass());
}

/// A breaker recovery probe must not be admitted under an
/// operator all-spawns pause — through the probe driver or the chokepoint —
/// however long it has been since the last probe.
#[tokio::test]
async fn breaker_recovery_probe_is_not_admitted_under_operator_all_spawns() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "ordinary work");
    let db = Arc::new(db);
    let execution = create_ready_chore_execution(&db, &work_item_id);
    let coordinator = make_dispatchable_coordinator(db.clone(), 4);
    operator_pause(coordinator.as_ref(), DispatchPauseScope::AllSpawns);

    let hold = coordinator
        .dispatch_hold_for(&execution, DispatchAdmission::BreakerRecoveryProbe)
        .expect("the chokepoint refuses a probe under an operator pause");
    assert!(hold.detail.contains("never auto-probed"), "{}", hold.detail);
    let result = coordinator
        .force_dispatch(&execution.id, DispatchAdmission::BreakerRecoveryProbe)
        .await;
    assert!(result.is_err(), "force_dispatch must refuse the probe admission");

    let spawn_health = SpawnHealthTracker::new();
    let events = Arc::new(RecordingDispatchEventSink::default());
    maybe_admit_recovery_probe(&db, &coordinator, &spawn_health, events.as_ref(), 1_000_000).await;

    assert!(!spawn_health.is_probe_execution(&execution.id));
    assert_eq!(db.get_execution(&execution.id).unwrap().status, ExecutionStatus::Ready);
    assert!(
        events.events().await.is_empty(),
        "nothing was admitted, so nothing is reported"
    );
}

/// A breaker trip while the operator holds every spawn must neither replace
/// the operator's pause (a Breaker pause is auto-resumable by recovery
/// evidence) nor go unreported: the attention item is still raised.
#[tokio::test]
async fn breaker_trip_leaves_an_operator_all_spawns_pause_in_place() {
    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "ordinary work");
    let db = Arc::new(db);
    let execution = create_ready_chore_execution(&db, &work_item_id);
    let coordinator = make_dispatchable_coordinator(db.clone(), 4);
    operator_pause(coordinator.as_ref(), DispatchPauseScope::AllSpawns);
    let before = coordinator.dispatch_pause().expect("paused");

    let spawn_health = SpawnHealthTracker::with_config(3, 300).with_breaker_enabled(true);
    let sink = RecordingDispatchEventSink::new();
    let second = create_ready_chore_execution(&db, &work_item_id);
    trip_spawn_capability_circuit(
        &db,
        &coordinator,
        &sink,
        &spawn_health,
        TripSignal {
            tripping_execution_id: &execution.id,
            tripping_work_item_id: &work_item_id,
            distinct_work_items: 3,
            now_epoch_secs: 2_000_000_000,
        },
    )
    .await;

    trip_spawn_capability_circuit(
        &db,
        &coordinator,
        &sink,
        &spawn_health,
        TripSignal {
            tripping_execution_id: &second.id,
            tripping_work_item_id: &work_item_id,
            distinct_work_items: 3,
            now_epoch_secs: 2_000_000_000,
        },
    )
    .await;

    assert_eq!(
        coordinator.dispatch_pause().expect("still paused"),
        before,
        "the operator's pause, origin and scope must be untouched"
    );
    assert_eq!(db.list_attention_items(&execution.id).unwrap().len(), 1);
    assert!(db.list_attention_items(&second.id).unwrap().is_empty());
    let events = sink.events().await;
    assert!(
        events
            .iter()
            .filter(|e| e.stage == "spawn_capability_unhealthy")
            .count()
            == 1,
        "the trip itself is still reported"
    );
    assert!(
        events.iter().all(|e| e.stage != "dispatch_paused"),
        "no breaker pause episode was started"
    );
    assert!(events.iter().any(|e| e.details["operator_all_spawns_hold"] == true));
    coordinator.resume_dispatch();
    coordinator.pause_dispatch_scoped(
        before.since_epoch_s + 1,
        DispatchPauseOrigin::Operator,
        DispatchPauseScope::AllSpawns,
        PauseReason::new("another hold").unwrap(),
    );
    trip_spawn_capability_circuit(
        &db,
        &coordinator,
        &sink,
        &spawn_health,
        TripSignal {
            tripping_execution_id: &second.id,
            tripping_work_item_id: &work_item_id,
            distinct_work_items: 3,
            now_epoch_secs: 2_000_000_001,
        },
    )
    .await;
    assert_eq!(db.list_attention_items(&second.id).unwrap().len(), 1);
}

/// The scope text names the scope for every origin/scope pairing, and agrees
/// with `reviews_held` so the words cannot drift from the behaviour.
#[test]
fn scope_description_states_the_scope_and_agrees_with_reviews_held() {
    let pause = |origin, scope| DispatchPause {
        origin,
        scope,
        since_epoch_s: 0,
        reason: "r".to_owned(),
    };
    let operator_default = pause(DispatchPauseOrigin::Operator, DispatchPauseScope::NewWork);
    let operator_all = pause(DispatchPauseOrigin::Operator, DispatchPauseScope::AllSpawns);
    let breaker = pause(DispatchPauseOrigin::Breaker, DispatchPauseScope::AllSpawns);

    assert!(!operator_default.reviews_held());
    assert!(operator_default.scope_description().contains("exempt"));
    assert!(operator_all.reviews_held());
    assert!(operator_all.scope_description().contains("every spawn is held"));
    assert!(operator_all.scope_description().contains("operator"));
    assert!(breaker.reviews_held());
    assert!(breaker.scope_description().contains("breaker"));
    assert_ne!(operator_all.scope_description(), breaker.scope_description());

    // A breaker pause defaults to all spawns; an operator pause to new work.
    assert_eq!(
        DispatchPauseOrigin::Breaker.default_scope(),
        DispatchPauseScope::AllSpawns
    );
    assert_eq!(
        DispatchPauseOrigin::Operator.default_scope(),
        DispatchPauseScope::NewWork
    );
}
