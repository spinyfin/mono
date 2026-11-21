use super::*;

fn deadline_sweep(
    db: &Arc<WorkDb>,
    states: &Arc<LiveWorkerStateRegistry>,
    coordinator: &Arc<ExecutionCoordinator>,
) -> ProgressionTimeoutSweep {
    ProgressionTimeoutSweep::builder()
        .work_db(db.clone())
        .live_states(states.clone())
        .coordinator(coordinator.clone())
        .pane_releaser(Arc::new(RecordingReaper::new(coordinator.clone(), db.clone())))
        .viewers(Arc::new(NoViewers))
        .hold_registry(Arc::new(crate::hold_registry::HoldRegistry::new()))
        .dispatch_events(Arc::new(crate::dispatch_events::NoopDispatchEventSink))
        .build()
}

#[tokio::test]
async fn viewer_retry_succeeds_after_production_reaper_clears_identity() {
    let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(60)).await;
    let mut sweep = deadline_sweep(&db, &states, &coordinator);
    sweep.viewers = Arc::new(UnconfirmedViewer);
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    assert_eq!(sweep.run_one_pass(now).await, 0);
    assert!(db.tmux_identity_for_execution(&execution).unwrap().is_none());
    assert_eq!(
        sweep.pane_releaser.release_pane(&execution).await,
        PaneReleaseOutcome::NoLiveWorker
    );
    sweep.viewers = Arc::new(NoViewers);
    let finished = db.get_execution(&execution).unwrap().finished_epoch().unwrap();
    assert_eq!(
        sweep
            .run_one_pass(finished + crate::pool_claim_sweep::LEAK_GRACE_SECS)
            .await,
        1
    );
    assert!(coordinator.progression_worker_pool().claims().await.is_empty());
}

#[tokio::test]
async fn orphan_without_identity_releases_after_terminal_grace() {
    let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(60)).await;
    db.clear_tmux_identity_for_execution(&execution, "deadline-token")
        .unwrap();
    db.mark_execution_orphaned(&execution, "lost pane").unwrap();
    let mut sweep = deadline_sweep(&db, &states, &coordinator);
    sweep.pane_releaser = Arc::new(UnconfirmedProcess);
    let finished = db.get_execution(&execution).unwrap().finished_epoch().unwrap();
    assert_eq!(sweep.run_one_pass(finished).await, 0);
    assert_eq!(
        sweep
            .run_one_pass(finished + crate::pool_claim_sweep::LEAK_GRACE_SECS - 1)
            .await,
        0
    );
    assert_eq!(
        sweep
            .run_one_pass(finished + crate::pool_claim_sweep::LEAK_GRACE_SECS)
            .await,
        1
    );
    assert!(states.get(SLOT).is_none());
    assert!(coordinator.progression_worker_pool().claims().await.is_empty());
}

#[tokio::test]
async fn fresh_unstarted_claim_ignores_old_queue_time() {
    let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(60)).await;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'claimed', started_at = NULL, created_at = '1' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
    let sweep = deadline_sweep(&db, &states, &coordinator);
    assert_eq!(sweep.run_one_pass(1000).await, 0);
    assert_eq!(sweep.run_one_pass(1059).await, 0);
    assert_eq!(db.get_execution(&execution).unwrap().status, ExecutionStatus::Claimed);
    assert_eq!(sweep.run_one_pass(1060).await, 1);
    assert_eq!(db.get_execution(&execution).unwrap().status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn holds_suspend_deadlines_and_timeout_emits_dispatch_details() {
    let (_dir, db, item, execution, states, coordinator) = progression_fixture(Some(60)).await;
    let mut sweep = deadline_sweep(&db, &states, &coordinator);
    let events = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    sweep.dispatch_events = events.clone();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    sweep.hold_registry.hold(&execution, None, now);
    assert_eq!(sweep.run_one_pass(now).await, 0);
    assert!(events.events().await.is_empty());
    sweep.hold_registry = Arc::new(crate::hold_registry::HoldRegistry::new());
    assert_eq!(sweep.run_one_pass(now).await, 1);
    let emitted = events.events_for(&execution).await;
    assert_eq!(emitted.len(), 1);
    let json = serde_json::to_value(&emitted[0]).unwrap();
    assert_eq!(json["work_item_id"], item);
    assert_eq!(json["details"]["reason"], "progression_run_timeout");
    assert_eq!(json["details"]["timeout_secs"], 60);
}

#[tokio::test]
async fn declaration_committed_after_observation_wins_deadline_transaction() {
    let (_dir, db, _item, execution, _states, _coordinator) = progression_fixture(Some(60)).await;
    assert!(db.progression_disposition(&execution).unwrap().is_none());
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET progression_disposition = 'no_action' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
    let completed = db
        .finish_progression_at_deadline(&execution, "progression-run-timeout")
        .unwrap()
        .unwrap();
    assert_eq!(completed.status, ExecutionStatus::Completed);
    assert_eq!(
        db.progression_disposition(&execution).unwrap(),
        Some(boss_protocol::ProgressionDisposition::NoAction)
    );
    assert!(
        db.list_runs(&execution)
            .unwrap()
            .iter()
            .all(|run| run.status != "failed")
    );
}
