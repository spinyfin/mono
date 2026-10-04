//! Regression coverage for stale viewer cleanup and bounded SlotBusy recovery.
use super::worker_pane_reattach::{answer_list_hosted_panes, install_tmux_override, seed_tmux_hosted_live_run};
use super::*;
use crate::spawn_flow::WorkerSpawner;

#[tokio::test]
async fn adoption_of_dead_registered_worker_preserves_owning_teardown() {
    use super::tmux_stub::{failure, fake_tmux, ok};
    let (state, _dir) = test_server_state();
    let run = seed_tmux_hosted_live_run(&state, 1, "dead-session", "dead-token");
    let pool = state.execution_coordinator.worker_pool();
    assert_eq!(pool.claim_worker(&run, None).await.unwrap(), "worker-1");
    state.worker_registry.register_tmux_run_slot(&run, 1, "dead-session");
    let (tmux, runner) = fake_tmux([
        ok("dead-session\t\n"),
        ok("BOSS_SPAWN_TOKEN=dead-token\n"),
        ok(&format!(
            "BOSS_SESSION_SCHEMA={}\n",
            crate::spawn_flow::TMUX_SESSION_SCHEMA
        )),
        ok("1"),
        ok("127"),
        ok("worker exited"),
        ok("BOSS_SPAWN_TOKEN=dead-token\n"),
        ok(""),
        failure("can't find session: dead-session"),
    ]);
    state.set_tmux_override_for_test(tmux.clone());
    let outcome = crate::tmux_adoption::run_adoption_pass(
        &state.work_db,
        &tmux,
        &state.execution_coordinator,
        state.as_ref(),
        &crate::worker_readoption::NoopLiveWorkerConvergence,
        &crate::dispatch_events::RecordingDispatchEventSink::new(),
    )
    .await;
    assert_eq!(outcome.dead_panes, 1);
    assert!(state.work_db.get_execution(&run).unwrap().status.is_terminal());
    assert!(state.work_db.tmux_identity_for_execution(&run).unwrap().is_some());
    assert!(state.live_worker_states.get(1).is_some());
    assert_eq!(pool.claims().await.len(), 1);
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    let releasing = state.clone();
    let releasing_run = run.clone();
    let release = tokio::spawn(async move { releasing.release_worker_pane(&releasing_run).await });
    answer_detach(&state, &sink, 1).await;
    assert_eq!(release.await.unwrap(), PaneReleaseOutcome::Reaped);
    assert!(state.live_worker_states.get(1).is_none());
    assert!(pool.claims().await.is_empty());
    assert!(state.work_db.tmux_identity_for_execution(&run).unwrap().is_none());
    assert_eq!(runner.calls().len(), 9);
}

pub(super) async fn answer_detach(state: &ServerState, sink: &SessionSink, slot: u8) {
    let envelope = tokio::time::timeout(Duration::from_secs(2), sink.next())
        .await
        .unwrap()
        .unwrap();
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest {
            request_id,
            request: EngineToAppRequest::DetachWorkerPane(input),
        } => {
            assert_eq!(input.slot_id, slot);
            request_id
        }
        other => panic!("expected detach, got {other:?}"),
    };
    state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::DetachWorkerPane {
                result: Ok(crate::protocol::DetachWorkerPaneResult {}),
            },
        )
        .await;
}

async fn answer_attach(
    state: &ServerState,
    sink: &SessionSink,
    result: Result<crate::protocol::AttachWorkerPaneResult, EngineToAppError>,
) {
    let envelope = tokio::time::timeout(Duration::from_secs(2), sink.next())
        .await
        .unwrap()
        .unwrap();
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest {
            request_id,
            request: EngineToAppRequest::AttachWorkerPane(input),
        } => {
            assert!(!input.run_id.is_empty());
            assert_eq!(input.slot_id, 3);
            request_id
        }
        other => panic!("expected attach, got {other:?}"),
    };
    state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::AttachWorkerPane { result },
        )
        .await;
}

fn attach_request() -> EngineToAppRequest {
    EngineToAppRequest::AttachWorkerPane(crate::protocol::AttachWorkerPaneInput {
        run_id: "new-run".into(),
        slot_id: 3,
        session_name: "new-session".into(),
        tmux_socket_path: boss_tmux::TEST_SOCKET_PATH.into(),
        summary: None,
        task_title: Some("new worker".into()),
    })
}

#[tokio::test]
async fn lost_detach_is_reconciled_on_registration_even_without_live_candidates() {
    let (state, _dir) = test_server_state();
    let run = seed_tmux_hosted_live_run(&state, 3, "old-session", "old-token");
    state.work_db.mark_execution_orphaned(&run, "finished offline").unwrap();
    super::tmux_stub::install_teardown(&state, &run, 4_194_303);
    state.worker_registry.register_run_slot(&run, 3);
    state.release_worker_pane(&run).await;
    assert!(state.live_worker_states.get(3).is_none());
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    let reconnecting = state.clone();
    let pass = tokio::spawn(async move { reconnecting.reattach_worker_panes_to_registered_app().await });
    answer_list_hosted_panes(&state, &sink, vec![(run, 3), ("unknown-run".into(), 4)]).await;
    answer_detach(&state, &sink, 3).await;
    answer_detach(&state, &sink, 4).await;
    pass.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn terminal_or_unknown_busy_occupant_is_detached_then_attach_retried() {
    for unknown in [false, true] {
        let (state, _dir) = test_server_state();
        let occupant = if unknown {
            "unknown-run".into()
        } else {
            let run = seed_tmux_hosted_live_run(&state, 3, "old-session", "old-token");
            state.work_db.mark_execution_orphaned(&run, "finished").unwrap();
            run
        };
        let sink = make_session_sink();
        state.register_app_session("session-app".into(), sink.clone()).await;
        let attaching = state.clone();
        let pass = tokio::spawn(async move {
            attaching
                .send_to_app_request(attach_request(), Duration::from_secs(1))
                .await
        });
        answer_attach(
            &state,
            &sink,
            Err(EngineToAppError::SlotBusy {
                occupying_run_id: Some(occupant),
            }),
        )
        .await;
        answer_detach(&state, &sink, 3).await;
        answer_attach(&state, &sink, Ok(crate::protocol::AttachWorkerPaneResult {})).await;
        assert!(matches!(
            pass.await.unwrap(),
            Ok(EngineToAppResponse::AttachWorkerPane { result: Ok(_) })
        ));
    }
}

#[tokio::test]
async fn live_different_occupant_is_not_detached_and_returns_typed_failure() {
    let (state, _dir) = test_server_state();
    let occupant = seed_tmux_hosted_live_run(&state, 3, "live-session", "live-token");
    let new_run = seed_tmux_hosted_live_run(&state, 4, "new-session", "new-token");
    let EngineToAppRequest::AttachWorkerPane(mut input) = attach_request() else {
        unreachable!()
    };
    input.run_id = new_run.clone();
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    let attaching = state.clone();
    let pass = tokio::spawn(async move {
        attaching
            .send_to_app_request(EngineToAppRequest::AttachWorkerPane(input), Duration::from_secs(1))
            .await
    });
    answer_attach(
        &state,
        &sink,
        Err(EngineToAppError::SlotBusy {
            occupying_run_id: Some(occupant.clone()),
        }),
    )
    .await;
    assert!(matches!(pass.await.unwrap(), Ok(EngineToAppResponse::AttachWorkerPane {
        result: Err(EngineToAppError::SlotBusy { occupying_run_id: Some(id) })
    }) if id == occupant));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.next())
            .await
            .is_err()
    );
    assert_eq!(state.live_worker_states.get(3).unwrap().run_id, occupant);
    assert!(
        state.work_db.list_attention_items(&new_run).unwrap().is_empty(),
        "attach_worker_viewer must not file pane_spawn_failed; the spawn coordinator owns that"
    );
}

#[tokio::test]
async fn same_occupant_slot_busy_is_treated_as_already_attached() {
    let (state, _dir) = test_server_state();
    let occupant = seed_tmux_hosted_live_run(&state, 3, "live-session", "live-token");
    let EngineToAppRequest::AttachWorkerPane(mut input) = attach_request() else {
        unreachable!()
    };
    input.run_id = occupant.clone();
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    let attaching = state.clone();
    let pass = tokio::spawn(async move {
        attaching
            .send_to_app_request(EngineToAppRequest::AttachWorkerPane(input), Duration::from_secs(1))
            .await
    });
    answer_attach(
        &state,
        &sink,
        Err(EngineToAppError::SlotBusy {
            occupying_run_id: Some(occupant.clone()),
        }),
    )
    .await;
    assert!(matches!(
        pass.await.unwrap(),
        Ok(EngineToAppResponse::AttachWorkerPane { result: Ok(_) })
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.next())
            .await
            .is_err(),
        "already-attached occupant must not be detached"
    );
    assert!(state.work_db.list_attention_items(&occupant).unwrap().is_empty());
}

#[tokio::test]
async fn busy_retry_is_bounded_and_failure_retains_original_occupant() {
    let (state, _dir) = test_server_state();
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    let attaching = state.clone();
    let pass = tokio::spawn(async move {
        attaching
            .send_to_app_request(attach_request(), Duration::from_secs(1))
            .await
    });
    let busy = EngineToAppError::SlotBusy {
        occupying_run_id: Some("unknown-run".into()),
    };
    answer_attach(&state, &sink, Err(busy.clone())).await;
    answer_detach(&state, &sink, 3).await;
    answer_attach(&state, &sink, Err(busy.clone())).await;
    assert_eq!(
        pass.await.unwrap().unwrap(),
        EngineToAppResponse::AttachWorkerPane { result: Err(busy) }
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn abort_reaps_only_the_new_process_without_detaching_the_occupant() {
    let (state, _dir) = test_server_state();
    let new_run = super::tmux_stub::seed_teardown(&state);
    let occupant = seed_tmux_hosted_live_run(&state, 3, "live-session", "live-token");
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    state.abort_worker_spawn(&new_run).await.unwrap();
    assert!(state.work_db.tmux_identity_for_execution(&new_run).unwrap().is_none());
    assert_eq!(state.live_worker_states.get(3).unwrap().run_id, occupant);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.next())
            .await
            .is_err()
    );
}

async fn answer_detach_unconfirmed(state: &ServerState, sink: &SessionSink, slot: u8) {
    let envelope = tokio::time::timeout(Duration::from_secs(2), sink.next())
        .await
        .unwrap()
        .unwrap();
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest {
            request_id,
            request: EngineToAppRequest::DetachWorkerPane(input),
        } => {
            assert_eq!(input.slot_id, slot);
            request_id
        }
        other => panic!("expected detach, got {other:?}"),
    };
    state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::DetachWorkerPane {
                result: Err(EngineToAppError::Timeout),
            },
        )
        .await;
}

#[tokio::test]
async fn unconfirmed_stale_detach_still_attaches_other_live_workers() {
    let (state, _dir) = test_server_state();
    let live = seed_tmux_hosted_live_run(&state, 3, "live-session", "live-token");
    let stale = seed_tmux_hosted_live_run(&state, 4, "stale-session", "stale-token");
    state.work_db.mark_execution_orphaned(&stale, "finished").unwrap();
    install_tmux_override(&state);
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    let reconnecting = state.clone();
    let pass = tokio::spawn(async move { reconnecting.reattach_worker_panes_to_registered_app().await });
    answer_list_hosted_panes(&state, &sink, vec![(stale, 4)]).await;
    answer_detach_unconfirmed(&state, &sink, 4).await;
    answer_attach(&state, &sink, Ok(crate::protocol::AttachWorkerPaneResult {})).await;
    pass.await.unwrap();
    assert_eq!(state.live_worker_states.get(3).unwrap().run_id, live);
}

#[tokio::test]
async fn failed_inventory_still_reattaches_live_workers() {
    let (state, _dir) = test_server_state();
    seed_tmux_hosted_live_run(&state, 3, "live-session", "live-token");
    install_tmux_override(&state);
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    let reconnecting = state.clone();
    let pass = tokio::spawn(async move { reconnecting.reattach_worker_panes_to_registered_app().await });
    let envelope = tokio::time::timeout(Duration::from_secs(2), sink.next())
        .await
        .unwrap()
        .unwrap();
    let FrontendEvent::EngineRequest {
        request_id,
        request: EngineToAppRequest::ListHostedPanes(_),
    } = envelope.payload
    else {
        panic!("expected inventory request")
    };
    state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::ListHostedPanes {
                result: Err(EngineToAppError::Timeout),
            },
        )
        .await;
    answer_attach(&state, &sink, Ok(crate::protocol::AttachWorkerPaneResult {})).await;
    pass.await.unwrap();
}

#[tokio::test]
async fn registered_inventory_failure_does_not_confirm_handback() {
    use crate::pool_claim_sweep::WorkerViewerDetach;
    let (state, _dir) = test_server_state();
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    let sweeping = state.clone();
    let pass = tokio::spawn(async move { sweeping.confirm_viewers_detached(&["finished-run".into()]).await });
    let envelope = tokio::time::timeout(Duration::from_secs(2), sink.next())
        .await
        .unwrap()
        .unwrap();
    let FrontendEvent::EngineRequest {
        request_id,
        request: EngineToAppRequest::ListHostedPanes(_),
    } = envelope.payload
    else {
        panic!("expected inventory request")
    };
    state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::ListHostedPanes {
                result: Err(EngineToAppError::Timeout),
            },
        )
        .await;
    assert!(pass.await.unwrap()[0].is_err());
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_stale_detach_retries_while_app_session_stays_registered() {
    let (state, _dir) = test_server_state();
    let stale = seed_tmux_hosted_live_run(&state, 4, "stale-session", "stale-token");
    state.work_db.mark_execution_orphaned(&stale, "finished").unwrap();
    let sink = make_session_sink();
    state.register_app_session("session-app".into(), sink.clone()).await;
    let reconnecting = state.clone();
    let pass = tokio::spawn(async move { reconnecting.reattach_worker_panes_to_registered_app().await });
    answer_list_hosted_panes(&state, &sink, vec![(stale.clone(), 4)]).await;
    answer_detach_unconfirmed(&state, &sink, 4).await;
    pass.await.unwrap();

    let retrying = state.clone();
    let retry_sink = sink.clone();
    let retry_stale = stale.clone();
    let retry = tokio::spawn(async move {
        answer_list_hosted_panes(&retrying, &retry_sink, vec![(retry_stale, 4)]).await;
        answer_detach_unconfirmed(&retrying, &retry_sink, 4).await;
    });
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::timeout(Duration::from_secs(2), retry)
        .await
        .expect("retry must inventory again while the app session stays registered")
        .unwrap();
}
