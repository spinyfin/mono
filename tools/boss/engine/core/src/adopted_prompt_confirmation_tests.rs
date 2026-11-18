use super::*;
use crate::live_worker_state::{LiveSpawnRouting, LiveWorkerStateRegistry, ReadoptionEvidence};
use crate::test_support::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[tokio::test]
async fn pending_confirmation_is_single_flight_and_does_not_delay_driver_start_reap() {
    let (dir, db) = open_db_arc();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Pending adopted prompt");
    let ready = create_ready_chore_execution(&db, chore.id);
    let (adopted, _) = db
        .start_execution_run(
            &ready.id,
            "worker-1",
            "mono",
            "lease",
            "workspace",
            dir.path().to_str().unwrap(),
        )
        .unwrap();
    db.begin_prompt_confirmation(&adopted.id).unwrap();
    let states = Arc::new(LiveWorkerStateRegistry::new());
    states.register_readoption(
        1,
        &adopted.id,
        "sonnet",
        1234,
        None,
        true,
        LiveSpawnRouting::none(),
        ReadoptionEvidence::DriverHook,
    );

    let silent_chore = create_test_chore(&db, product.id, "Driver never started");
    let silent = create_old_execution(&db, &silent_chore.id);
    states.register_spawn_with_capabilities(2, &silent, "sonnet", 1235, None, true, LiveSpawnRouting::none());
    states.set_spawn_time_for_test(2, boss_engine_utils::epoch_time::now_epoch_secs() - 400);
    let coordinator = make_coordinator(db.clone(), 2);
    coordinator.worker_pool().claim_worker(&adopted.id, None).await;
    coordinator.worker_pool().claim_worker(&silent, None).await;
    let reaper = Arc::new(super::tests::Reaper {
        states: states.clone(),
        accept: false,
        returns: AtomicUsize::new(0),
        reaps: AtomicUsize::new(0),
    });
    let services = Arc::new(
        ConfirmationServices::builder()
            .work_db(db.clone())
            .live_states(states)
            .coordinator(coordinator)
            .dispatch_events(Arc::new(crate::dispatch_events::RecordingDispatchEventSink::default()))
            .reaper(reaper.clone())
            .spawn_health(Arc::new(crate::spawn_health::SpawnHealthTracker::default()))
            .cube_client(Arc::new(super::tests::TestCube::default()))
            .build(),
    );
    let mut confirmations = Confirmations::default();
    confirmations.schedule(services.clone());
    tokio::task::yield_now().await;
    let task = confirmations.0[&adopted.id].id();
    confirmations.schedule(services.clone());
    assert_eq!(confirmations.0.len(), 1);
    assert_eq!(confirmations.0[&adopted.id].id(), task);
    let ctx = services.context();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        crate::spawn_ack_sweep::run_one_pass(
            ctx.work_db,
            ctx.live_states,
            ctx.coordinator.clone(),
            ctx.dispatch_events,
            ctx.reaper,
            ctx.spawn_health,
            ctx.cube_client,
            300,
        ),
    )
    .await
    .expect("pending prompt must not hold the sweep's driver-start reap or recovery probe");
    assert_eq!(outcome.driver_start_reaped, 1);
    assert_eq!(reaper.reaps.load(Ordering::SeqCst), 1);
    assert!(!confirmations.0[&adopted.id].is_finished());
    assert!(db.prompt_confirmation_pending(&adopted.id).unwrap());
    // Dropping the sweep's owner cancels its tracked task, rather than
    // detaching a multi-minute wait that can reap after sweep shutdown.
    let abort = confirmations.0[&adopted.id].abort_handle();
    drop(confirmations);
    tokio::task::yield_now().await;
    assert!(abort.is_finished());
}
