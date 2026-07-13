use super::*;

// Tests for `ServerState::retire_pane` / `ServerState::list_husk_panes` —
// the break-glass "husk pane" verb. A husk is a pane the app still hosts
// a session in that the engine has NO live-tracked run for (crash,
// terminal-fail path bug, spawn-ack timeout); neither `stop_run` nor
// `reap_run` can reach it since both resolve through a run id the
// engine no longer maps to a slot.

#[tokio::test]
async fn retire_pane_refuses_when_live_run_tracked_in_slot() {
    // Safety check: a slot the engine's own LiveWorkerStateRegistry
    // still considers live (non-terminal) is NOT a husk. Retiring it
    // would tear down a pane the engine thinks is doing work — the
    // caller must go through `agents stop` instead.
    let server_state = test_server_state();
    server_state
        .live_worker_states
        .register_spawn(3, "run-live", "claude-opus-4-7", 0, None);

    let result = server_state.retire_pane(3).await;
    match result {
        Err(RetirePaneError::LiveRunTracked { slot_id, run_id }) => {
            assert_eq!(slot_id, 3);
            assert_eq!(run_id, "run-live");
        }
        other => panic!("expected LiveRunTracked, got {other:?}"),
    }

    // The refusal must not have touched the live-state entry.
    assert!(
        server_state.live_worker_states.get(3).is_some(),
        "a refused retire must leave the live-tracked slot untouched"
    );
}

#[test]
fn retire_pane_error_message_points_at_agents_stop() {
    // The whole point of the safety check is to redirect the operator
    // to the right verb — pin the message text so a future refactor
    // can't silently drop the pointer.
    let err = RetirePaneError::LiveRunTracked {
        slot_id: 3,
        run_id: "run-live".to_owned(),
    };
    let message = err.to_string();
    assert!(
        message.contains("agents stop"),
        "message should point at `agents stop`: {message}"
    );
    assert!(
        message.contains("run-live"),
        "message should name the tracked run: {message}"
    );
}

#[tokio::test]
async fn retire_pane_succeeds_for_husk_slot_with_no_app_session() {
    // No app session registered (headless/test engine): retire_pane
    // must still succeed — there's nothing to round-trip to, and the
    // engine-side cleanup (which is what this call chiefly guarantees
    // for a genuine husk) is unconditional.
    let server_state = test_server_state();

    let result = server_state.retire_pane(4).await;
    assert!(result.is_ok(), "expected Ok, got {result:?}");
}

#[tokio::test]
async fn retire_pane_sends_slot_keyed_release_request_with_no_run_id_resolution() {
    // The defining property of retire_pane vs release_worker_pane: it
    // never resolves through worker_registry (there is no run id for
    // a husk) — it goes straight to the app with the slot id the
    // caller supplied.
    let server_state = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let retire = tokio::spawn(async move { server_clone.retire_pane(7).await });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::ReleaseWorkerPane(input) => {
            assert_eq!(input.slot_id, 7);
        }
        other => panic!("expected ReleaseWorkerPane, got {other:?}"),
    }

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::ReleaseWorkerPane {
                result: Ok(crate::protocol::ReleaseWorkerPaneResult {}),
            },
        )
        .await;

    let result = retire.await.expect("retire task");
    assert!(result.is_ok(), "expected Ok, got {result:?}");
}

#[tokio::test]
async fn list_husk_panes_returns_empty_when_no_app_session_registered() {
    // Best-effort query: with no app session there is nothing to
    // diff, so this must not be a hard error.
    let server_state = test_server_state();
    let panes = server_state.list_husk_panes().await.expect("expected Ok");
    assert!(panes.is_empty());
}

#[tokio::test]
async fn list_husk_panes_filters_out_slots_the_engine_still_tracks_live() {
    // The app reports two hosted slots: one the engine still has a
    // live (non-terminal) run for — not a husk, must be filtered —
    // and one the engine has no live entry for at all — a genuine
    // husk, must be reported.
    let server_state = test_server_state();
    server_state
        .live_worker_states
        .register_spawn(2, "run-live", "claude-opus-4-7", 0, None);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let list = tokio::spawn(async move { server_clone.list_husk_panes().await });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, request } => {
            assert!(
                matches!(request, EngineToAppRequest::ListHostedPanes(_)),
                "expected ListHostedPanes, got {request:?}"
            );
            request_id
        }
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::ListHostedPanes {
                result: Ok(crate::protocol::ListHostedPanesResult {
                    panes: vec![
                        crate::protocol::HostedPaneEntry {
                            slot_id: 2,
                            run_id: "run-live".to_owned(),
                            summary: None,
                            task_title: None,
                        },
                        crate::protocol::HostedPaneEntry {
                            slot_id: 6,
                            run_id: "run-husk".to_owned(),
                            summary: Some("fixing the fencer scraper".to_owned()),
                            task_title: None,
                        },
                    ],
                }),
            },
        )
        .await;

    let panes = list.await.expect("list task").expect("expected Ok");
    assert_eq!(
        panes.len(),
        1,
        "only the non-live slot should be reported as a husk: {panes:?}"
    );
    assert_eq!(panes[0].slot_id, 6);
    assert_eq!(panes[0].run_id, "run-husk");
}
