//! App-only coordinator-chat RPC. This entry declares the surface and answers
//! that the chat backend is not running; a later entry wires the service.

use super::*;
use boss_protocol::CoordinatorChatOutcome;

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
    let app_session_id = server_state
        .app_session
        .lock()
        .await
        .as_ref()
        .map(|handle| handle.session_id.clone());
    if app_session_id.as_deref() != Some(session_id.as_str()) {
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
