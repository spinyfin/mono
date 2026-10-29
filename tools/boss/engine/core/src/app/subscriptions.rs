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
    {
        let wants_decisions = topics.iter().any(|topic| topic.trim() == decision_notifications::TOPIC);
        let wants_background_work = topics
            .iter()
            .any(|topic| topic.trim() == boss_protocol::TOPIC_ENGINE_BACKGROUND_WORK);
        let topics = server_state.topic_broker.subscribe(&session_id, &topics).await;
        if wants_decisions {
            server_state
                .topic_broker
                .refresh_decisions(&server_state.work_db, true)
                .await;
        }
        if wants_background_work {
            // Like the decision feed, a re-subscribe (resync) replays state.
            // Sent to this session only: the others already hold it.
            server_state
                .topic_broker
                .replay_background_work(&server_state.work_db, &sink)
                .await;
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
        send_response(&sink, &request_id, FrontendEvent::Unsubscribed { topics });
    }
}
