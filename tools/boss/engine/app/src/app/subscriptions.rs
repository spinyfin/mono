//! `FrontendRequest` handlers — topic subscribe/unsubscribe.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

pub(super) async fn handle_subscribe(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::Subscribe { topics } = req else {
        unreachable!()
    };
    // The coordinator-chat topic is app-only, independent of worker RPC
    // tiering. The broker trims topic names, so compare the trimmed name.
    if topics
        .iter()
        .any(|topic| topic.trim() == boss_protocol::TOPIC_COORDINATOR_CHAT)
        && !coordinator_chat::is_app_session(&server_state, &session_id).await
    {
        send_response(
            &sink,
            &request_id,
            FrontendEvent::Error {
                message: "subscribe: only the app session may subscribe to coordinator.chat".to_owned(),
            },
        );
        return;
    }
    {
        let wants_decisions = topics.iter().any(|topic| topic.trim() == decision_notifications::TOPIC);
        let topics = server_state.topic_broker.subscribe(&session_id, &topics).await;
        if wants_decisions {
            server_state
                .topic_broker
                .refresh_decisions(&server_state.work_db, true)
                .await;
        }
        // A pane-capture topic is a request to watch: start (or join) the
        // run's capture. Only newly added topics count, so a repeat
        // subscribe does not start a second one.
        for run_id in topics
            .iter()
            .filter_map(|topic| boss_protocol::pane_capture_topic_run(topic))
        {
            server_state.pane_viewers.viewer_joined(&server_state, run_id).await;
        }
        send_response(
            &sink,
            &request_id,
            FrontendEvent::Subscribed {
                topics,
                current_revision: server_state.current_work_revision(),
            },
        );
    }
}

pub(super) async fn handle_unsubscribe(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        session_id,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::Unsubscribe { topics } = req else {
        unreachable!()
    };
    {
        let topics = server_state.topic_broker.unsubscribe(&session_id, &topics).await;
        if topics
            .iter()
            .any(|topic| boss_protocol::pane_capture_topic_run(topic).is_some())
        {
            server_state.pane_viewers.viewers_may_have_left().await;
        }
        send_response(&sink, &request_id, FrontendEvent::Unsubscribed { topics });
    }
}
