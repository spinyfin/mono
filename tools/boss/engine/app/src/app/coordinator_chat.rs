//! App-only coordinator-chat RPC. Until a chat backend is attached, every
//! operation from the app session is answered with `BackendNotRunning`.
//!
//! The session check below is the authoritative app-only gate. The request
//! contract only excludes worker and progression tiers (as for
//! `RecreateCoordinator`), and it applies only when worker RPC tiering is on,
//! so this check must not rely on it.

use super::*;
use boss_protocol::CoordinatorChatOutcome;

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
    let FrontendRequest::CoordinatorChat { .. } = req else {
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
    send_response(
        &sink,
        &request_id,
        FrontendEvent::CoordinatorChatResult {
            outcome: CoordinatorChatOutcome::BackendNotRunning {
                reason: "the chat backend is not running".to_owned(),
            },
        },
    );
}
