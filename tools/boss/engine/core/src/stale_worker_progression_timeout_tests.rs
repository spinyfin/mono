//! `progression.run_timeout` applied through the stale-worker sweep: a
//! progression run older than the timeout is reaped through the sweep's
//! ordinary reaper and recorded `orphaned` (an infrastructure failure to the
//! candidate view), and the stuck item it inspects is never touched.
use super::*;

fn description_of(db: &WorkDb, id: &str) -> String {
    match db.get_work_item(id).unwrap() {
        boss_protocol::WorkItem::Chore(t) | boss_protocol::WorkItem::Task(t) => t.description,
        _ => panic!("expected a chore"),
    }
}

/// A live progression run started five minutes ago on an active chore, with
/// the engine's published timeout set to `timeout_secs`.
async fn progression_fixture(
    timeout_secs: Option<i64>,
) -> (
    tempfile::TempDir,
    Arc<WorkDb>,
    String,
    String,
    Arc<LiveWorkerStateRegistry>,
    Arc<ExecutionCoordinator>,
) {
    let (dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "stuck chore");
    let db = Arc::new(db);
    let execution_id = create_old_execution(&db, &work_item_id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'progression' WHERE id = ?1",
            [&execution_id],
        )
        .unwrap();
    if let Some(secs) = timeout_secs {
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO metadata (key, value) VALUES ('progression.run_timeout_secs', ?1)",
                [secs.to_string()],
            )
            .unwrap();
    }
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_slot(&live_states, 1, &execution_id, &work_item_id);
    drive_to_working_idle(&live_states, 1);
    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;
    (dir, db, work_item_id, execution_id, live_states, coordinator)
}

async fn sweep(
    db: &Arc<WorkDb>,
    live_states: &LiveWorkerStateRegistry,
    coordinator: &Arc<ExecutionCoordinator>,
    reaper: &RecordingReaper,
) -> StaleWorkerSweepOutcome {
    // Thresholds that would reap/flag any ordinary worker on sight: a
    // progression run must be governed by its own timeout instead.
    run_one_pass(
        db.as_ref(),
        live_states,
        coordinator.clone(),
        &RecordingDispatchEventSink::new(),
        reaper,
        &HoldRegistry::new(),
        ALWAYS_STALE,
    )
    .await
}

#[tokio::test]
async fn a_progression_run_past_its_timeout_is_reaped_orphaned_and_the_item_is_untouched() {
    let (_dir, db, work_item_id, execution_id, live_states, coordinator) = progression_fixture(Some(60)).await;
    let before = description_of(&db, &work_item_id);
    let reaper = RecordingReaper::new(coordinator.clone());

    let outcome = sweep(&db, &live_states, &coordinator, &reaper).await;

    assert_eq!(outcome.reaped, 1);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );
    assert_eq!(
        reaper.reaped(),
        vec![(execution_id.clone(), true)],
        "reaped through the sweep's ordinary reaper, before the slot is released"
    );
    assert!(
        !coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(&execution_id)
    );
    assert_eq!(description_of(&db, &work_item_id), before, "no audit line on the item");
    assert!(
        db.list_attention_items_for_work_item(&work_item_id)
            .unwrap()
            .iter()
            .all(|item| item.kind != STALE_WORKER_ATTENTION_KIND),
        "no stale-worker attention on the item"
    );
}

#[tokio::test]
async fn a_progression_run_inside_its_timeout_is_left_alone_even_when_it_looks_idle() {
    let (_dir, db, work_item_id, execution_id, live_states, coordinator) = progression_fixture(Some(3_600)).await;
    let before = description_of(&db, &work_item_id);
    let reaper = RecordingReaper::new(coordinator.clone());

    let outcome = sweep(&db, &live_states, &coordinator, &reaper).await;

    assert_eq!(outcome.reaped, 0);
    assert_eq!(outcome.genuinely_stuck, 0);
    assert_ne!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned
    );
    assert!(reaper.reaped().is_empty());
    assert_eq!(description_of(&db, &work_item_id), before);
    assert!(
        db.list_attention_items_for_work_item(&work_item_id)
            .unwrap()
            .iter()
            .all(|item| item.kind != STALE_WORKER_ATTENTION_KIND),
    );
}

#[test]
fn the_timeout_falls_back_to_the_placeholder_when_nothing_is_published() {
    let (_dir, db) = open_db();
    assert_eq!(progression_run_timeout_secs(&db), 30 * 60);
}

/// The published limit round-trips from the engine's settings.
#[test]
fn the_engine_publishes_the_timeout_the_sweep_reads() {
    let (_dir, db) = open_db();
    db.publish_progression_limits(
        &boss_engine_storage_values::progression::ProgressionSettings::builder()
            .run_timeout_secs(77)
            .build(),
    )
    .unwrap();
    assert_eq!(progression_run_timeout_secs(&db), 77);
}
