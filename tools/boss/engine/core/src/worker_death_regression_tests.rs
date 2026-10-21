use super::*;

#[tokio::test]
async fn missing_session_reconciles_without_a_final_transcript_event() {
    for activity in [
        WorkerActivity::Working,
        WorkerActivity::Idle,
        WorkerActivity::WaitingForInput,
    ] {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "missing session");
        let db = Arc::new(db);
        let execution_id = create_spawned_execution(&db, &work_item_id, i64::from(i32::MAX));
        let states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&states, 4, &execution_id, &work_item_id);
        drive_to_working_tool_in_flight(&states, 4);
        states.set_activity_for_test(4, activity);
        states.set_last_event_at_for_test(4, "2026-10-20T19:42:42Z");
        states.set_live_status(4, Some("Waiting on test suite completion".into()));
        let coordinator = make_coordinator(db.clone(), 4);
        let reaper = RecordingReaper::new(coordinator.clone());
        let sink = RecordingDispatchEventSink::new();
        let inspector = StaticTerminalInspector(TerminalLiveness::Dead {
            session_name: "missing-worker".into(),
            evidence: DeadPaneEvidence::SessionAbsent,
        });
        let holds = HoldRegistry::new();
        let result = run_one_pass_with_terminal(
            &db,
            &states,
            Some(&inspector),
            coordinator,
            &sink,
            StaleWorkerSweepControls {
                reaper: &reaper,
                hold_registry: &holds,
                cube_client: &NoopCube,
            },
            StaleWorkerThresholds {
                stale_threshold_secs: NEVER_STALE,
                auto_reap_threshold_secs: NEVER_STALE,
            },
        )
        .await;
        assert_eq!(result.dead, 1, "{activity:?}");
        assert_eq!(result.reaped, 1, "{activity:?}");
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned
        );
        assert!(states.get(4).is_none());
        assert!(!states.set_live_status(4, Some("stale summary".into())));
    }
}

#[test]
fn observed_death_freezes_summary_and_preserves_last_driver_event() {
    let states = LiveWorkerStateRegistry::new();
    states.register_spawn(4, "dead-run", "codex", 0, None);
    drive_to_working_tool_in_flight(&states, 4);
    states.set_last_event_at_for_test(4, "2026-10-20T19:42:42Z");
    states.set_live_status(4, Some("Waiting on test suite completion".into()));
    let before = states.get(4).unwrap();
    assert!(states.mark_terminated("dead-run"));
    drive_to_working_tool_in_flight(&states, 4);
    assert!(!states.set_live_status(4, Some("late model response".into())));
    let after = states.get(4).unwrap();
    assert_eq!(after.activity, WorkerActivity::Terminated);
    assert_eq!(after.last_event_at, before.last_event_at);
    assert_eq!(after.live_status, before.live_status);
    assert_eq!(after.live_status_at, before.live_status_at);
    assert!(after.current_tool.is_none());
}
