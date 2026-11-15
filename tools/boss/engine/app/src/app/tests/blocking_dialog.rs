use super::tmux_stub::{RecordingPaneRunner, TEST_SPAWN_TOKEN, tmux_with_runner};
use super::*;
use boss_engine_runtime::spawn_ack_sweep::SpawnAckReaper;

#[tokio::test]
async fn blocking_dialog_refuses_text_interrupt_and_recovery_return_and_fails_run() {
    for path in ["nudge", "interrupt", "return", "probe", "answer"] {
        let (state, _dir) = test_server_state();
        let run = register_idle_worker_with_driver(&state, 1, Some("claude"));
        state.worker_registry.register_tmux_run_slot(&run, 1, "boss-dialog");
        register_tmux_identity_for_test(&state, &run, "boss-dialog", TEST_SPAWN_TOKEN);
        let runner = Arc::new(
            RecordingPaneRunner::new("boss-dialog")
                .capture_text("Claude Code\nDo you want to proceed?\n1. Yes\n2. No\nEsc to cancel"),
        );
        *state.pane_delivery_tmux_override.write().unwrap() = Some(tmux_with_runner(runner.clone()));
        state.set_tmux_override_for_test(tmux_with_runner(runner.clone()));
        let error = match path {
            "nudge" => state
                .send_input_to_worker(&run, "3 never approve this".into())
                .await
                .unwrap_err()
                .to_string(),
            "interrupt" => state.interrupt_worker_pane(&run).await.unwrap_err().to_string(),
            "return" => state.resubmit_prompt(&run).await.unwrap_err().to_string(),
            "probe" | "answer" => {
                if path == "answer" {
                    state.queue_probe_resolving_worker_signal(run.clone(), "answer: continue".into(), false);
                } else {
                    state.queue_probe(run.clone(), "status?".into(), false);
                }
                super::super::worker_events::dispatch_probe_now(&state, &run).await;
                let boss_protocol::WorkItem::Chore(task) = state
                    .work_db
                    .get_work_item(&state.work_db.get_execution(&run).unwrap().work_item_id)
                    .unwrap()
                else {
                    panic!("expected chore");
                };
                task.blocked_detail.unwrap_or_default()
            }
            _ => unreachable!(),
        };
        assert!(error.contains("BlockingDialogOnScreen"), "{path}: {error}");
        assert!(error.contains("Do you want to proceed?"), "{path}: {error}");
        assert!(
            !runner.calls().iter().any(|call| call
                .iter()
                .any(|arg| matches!(arg.as_str(), "send-keys" | "paste-buffer" | "load-buffer"))),
            "{path}: {:?}",
            runner.calls()
        );
        assert_eq!(
            state.work_db.get_execution(&run).unwrap().status,
            boss_protocol::ExecutionStatus::Failed,
            "{path}"
        );
        assert!(state.worker_registry.slot_for_run(&run).is_none(), "{path}");
    }
}

#[tokio::test]
async fn normal_screen_allows_text_interrupt_and_recovery_return() {
    for path in ["nudge", "interrupt", "return", "probe", "answer"] {
        let (state, _dir) = test_server_state();
        let run = register_idle_worker_with_driver(&state, 1, Some("claude"));
        state.worker_registry.register_tmux_run_slot(&run, 1, "boss-clear");
        register_tmux_identity_for_test(&state, &run, "boss-clear", TEST_SPAWN_TOKEN);
        let runner = Arc::new(RecordingPaneRunner::new("boss-clear").echo_last_paste());
        *state.pane_delivery_tmux_override.write().unwrap() = Some(tmux_with_runner(runner.clone()));
        match path {
            "nudge" => {
                state.send_input_to_worker(&run, "status?".into()).await.unwrap();
            }
            "interrupt" => {
                state.interrupt_worker_pane(&run).await.unwrap();
            }
            "return" => {
                state.resubmit_prompt(&run).await.unwrap();
            }
            "probe" | "answer" => {
                let probe_id = if path == "answer" {
                    state.queue_probe_resolving_worker_signal(run.clone(), "answer: continue".into(), false)
                } else {
                    state.queue_probe(run.clone(), "status?".into(), false)
                };
                super::super::worker_events::dispatch_probe_now(&state, &run).await;
                assert_eq!(
                    state.probe_lifecycle_state(&probe_id),
                    Some(boss_protocol::ProbeDeliveryState::Consumed)
                );
            }
            _ => unreachable!(),
        }
        assert!(
            runner
                .calls()
                .iter()
                .any(|call| call.iter().any(|arg| arg == "send-keys")),
            "{path}"
        );
        assert!(
            !state.work_db.get_execution(&run).unwrap().status.is_terminal(),
            "{path}"
        );
    }
}

#[tokio::test]
async fn working_worker_dialog_abandons_mid_turn_and_after_interrupt_probes() {
    const DIALOG: &str = "Do you want to proceed?\n1. Yes\n2. No\nEsc to cancel";
    for interrupt in [false, true] {
        let (state, _dir) = test_server_state();
        let run = register_working_worker_with_driver(&state, 1, Some("claude"));
        state.worker_registry.register_tmux_run_slot(&run, 1, "boss-dialog");
        register_tmux_identity_for_test(&state, &run, "boss-dialog", TEST_SPAWN_TOKEN);
        let runner = RecordingPaneRunner::new("boss-dialog");
        let runner = Arc::new(if interrupt {
            runner.dialog_after_interrupt(DIALOG)
        } else {
            runner.capture_text(DIALOG)
        });
        *state.pane_delivery_tmux_override.write().unwrap() = Some(tmux_with_runner(runner.clone()));
        state.set_tmux_override_for_test(tmux_with_runner(runner.clone()));
        let probe = state.queue_probe(run.clone(), "status?".into(), false);
        if interrupt {
            let parked = super::probe_interrupt::park_once_interrupted(&state, &runner, 1);
            let outcome = super::super::probe_interrupt::deliver_probe_interrupting(&state, &run, &probe).await;
            parked.await.unwrap();
            assert_eq!(outcome.state, boss_protocol::ProbeDeliveryState::Abandoned);
            assert_eq!(outcome.interrupt, boss_protocol::ProbeInterruptOutcome::Interrupted);
            assert_eq!(runner.escape_presses(), 1);
        } else {
            assert_eq!(
                super::super::worker_events::dispatch_probe_now(&state, &run).await,
                super::super::probes::ProbeDispatchOutcome::Dispatched(boss_protocol::ProbeDeliveryState::Abandoned)
            );
        }
        assert!(!runner.wrote_text());
        assert_eq!(
            state.probe_lifecycle_state(&probe),
            Some(boss_protocol::ProbeDeliveryState::Abandoned)
        );
        assert!(!state.has_in_flight_probe(&run));
        assert_eq!(
            state.work_db.get_execution(&run).unwrap().status,
            boss_protocol::ExecutionStatus::Failed
        );
        assert!(state.worker_registry.slot_for_run(&run).is_none());
        let task = state.work_db.get_execution(&run).unwrap().work_item_id;
        let basis: String = state.work_db.connect().unwrap().query_row(
            "SELECT basis FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason' ORDER BY id DESC LIMIT 1",
            [&task], |row| row.get(0),
        ).unwrap();
        assert_eq!(basis, "blocking_dialog_on_screen");
    }
}

#[tokio::test]
async fn pane_input_uses_spawned_driver_and_fails_closed_for_unknown_identity() {
    for unknown in [false, true] {
        let (state, _dir) = test_server_state();
        let run = register_idle_worker_with_driver(&state, 1, Some("claude"));
        let task = state.work_db.get_execution(&run).unwrap().work_item_id;
        state
            .work_db
            .connect()
            .unwrap()
            .execute("UPDATE tasks SET driver = 'codex' WHERE id = ?1", [&task])
            .unwrap();
        if unknown {
            // Simulate a persisted driver whose implementation is no longer registered.
            state
                .work_db
                .connect()
                .unwrap()
                .execute(
                    "UPDATE work_executions SET driver = 'unknown-driver' WHERE id = ?1",
                    [&run],
                )
                .unwrap();
        }
        state.worker_registry.register_tmux_run_slot(&run, 1, "boss-dialog");
        register_tmux_identity_for_test(&state, &run, "boss-dialog", TEST_SPAWN_TOKEN);
        let runner = Arc::new(
            RecordingPaneRunner::new("boss-dialog")
                .capture_text("Do you want to proceed?\n1. Yes\n2. No\nEsc to cancel"),
        );
        *state.pane_delivery_tmux_override.write().unwrap() = Some(tmux_with_runner(runner.clone()));
        state.set_tmux_override_for_test(tmux_with_runner(runner.clone()));
        let error = state.send_pane_text_checked(&run, 1, "status?").await.unwrap_err();
        if unknown {
            assert!(matches!(
                error,
                super::super::pane_delivery::PaneSendFailure::DriverLivenessUnavailable(_)
            ));
            assert!(runner.calls().is_empty());
        } else {
            assert!(
                matches!(error, super::super::pane_delivery::PaneSendFailure::Tmux(ref error)
                if error.is::<boss_tmux::BlockingDialogOnScreen>())
            );
        }
        assert!(!runner.wrote_text());
    }
}
