//! App-only coordinator-chat RPC. The operations are served by the chat
//! service in `boss_engine_coordinator_chat`; this handler gates who may ask
//! and maps the service's answers onto the app protocol. While no chat service
//! is running (which is every time, until the coordinator lifecycle starts
//! one) every operation from the app session is answered with
//! `BackendNotRunning`.
//!
//! The session check below is the authoritative app-only gate. The request
//! contract only excludes worker and progression tiers (as for
//! `RecreateCoordinator`), and it applies only when worker RPC tiering is on,
//! so this check must not rely on it.

use super::*;
use boss_engine_coordinator_chat::ChatError;
use boss_protocol::CoordinatorChatOutcome;

/// Answer while there is no chat service. The panel shows it and offers the
/// terminal coordinator.
const NOT_RUNNING: &str = "the chat backend is not running";

/// True when `session_id` is the registered app session. False when no app
/// session is registered.
pub(super) async fn is_app_session(server_state: &ServerState, session_id: &str) -> bool {
    server_state
        .app_session
        .lock()
        .await
        .as_ref()
        .is_some_and(|handle| handle.session_id == session_id)
}

pub(super) async fn handle_coordinator_chat(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CoordinatorChat { operation } = req else {
        unreachable!()
    };
    if !is_app_session(&server_state, &session_id).await {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::Error {
                message: "coordinator_chat: only the app session may use coordinator chat".to_owned(),
            },
        );
        return;
    }
    let event = match server_state.coordinator_chat() {
        None => not_running(NOT_RUNNING.to_owned()),
        Some(backend) => match backend.handle(operation).await {
            Ok(outcome) => FrontendEvent::CoordinatorChatResult { outcome },
            Err(ChatError::NotRunning(reason)) => not_running(reason),
            // The intent log could not be written; the service has stopped.
            Err(ChatError::Storage(reason)) => not_running(reason),
            // A request that could not be honoured: the app shows it inline.
            Err(error) => FrontendEvent::Error {
                message: format!("coordinator_chat: {error}"),
            },
        },
    };
    send_response(&sink, &request_id, event);
}

fn not_running(reason: String) -> FrontendEvent {
    FrontendEvent::CoordinatorChatResult {
        outcome: CoordinatorChatOutcome::BackendNotRunning { reason },
    }
}
