//! Regression coverage for stale viewer cleanup and bounded SlotBusy recovery.
use super::worker_pane_reattach::{answer_list_hosted_panes, seed_tmux_hosted_live_run};
use super::*;
use crate::spawn_flow::WorkerSpawner;

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
    let attentions = state.work_db.list_attention_items(&new_run).unwrap();
    assert_eq!(attentions.len(), 1);
    assert!(attentions[0].body_markdown.contains(&occupant));
    assert_eq!(attentions[0].status, "open");
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
