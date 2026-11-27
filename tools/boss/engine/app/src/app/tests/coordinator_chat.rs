use super::*;
use crate::app::coordinator_chat::handle_coordinator_chat;
use boss_protocol::{CoordinatorChatOperation, CoordinatorChatOutcome};

fn dispatch(state: &Arc<ServerState>, sink: &Arc<SessionSink>, session_id: &str) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id(session_id)
        .request_id("req-1")
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

fn chat(operation: CoordinatorChatOperation) -> FrontendRequest {
    FrontendRequest::CoordinatorChat { operation }
}

fn operations() -> [CoordinatorChatOperation; 4] {
    [
        CoordinatorChatOperation::Send {
            text: "hi".into(),
            client_message_id: "c1".into(),
            attachments: vec![],
        },
        CoordinatorChatOperation::Cancel,
        CoordinatorChatOperation::AnswerPermission {
            request_id: "0".into(),
            option_id: "allow-once".into(),
        },
        CoordinatorChatOperation::Fetch { after_seq: 0 },
    ]
}

async fn sole_response(sink: &SessionSink) -> FrontendEvent {
    sink.close();
    let response = sink.next().await.expect("handler must send a response").payload;
    assert!(
        sink.next().await.is_none(),
        "handler must send exactly one response, got a second",
    );
    response
}

#[tokio::test]
async fn app_session_is_told_the_chat_backend_is_not_running() {
    let (server_state, _dir) = test_server_state();
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    for operation in operations() {
        let sink = make_session_sink();
        handle_coordinator_chat(dispatch(&server_state, &sink, "session-app"), chat(operation)).await;
        match sole_response(&sink).await {
            FrontendEvent::CoordinatorChatResult {
                outcome: CoordinatorChatOutcome::BackendNotRunning { reason },
            } => {
                assert_eq!(reason, "the chat backend is not running");
            }
            other => panic!("expected backend-not-running, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_non_app_session_is_rejected() {
    let (server_state, _dir) = test_server_state();
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    for operation in operations() {
        let sink = make_session_sink();
        handle_coordinator_chat(dispatch(&server_state, &sink, "session-cli"), chat(operation)).await;
        match sole_response(&sink).await {
            FrontendEvent::Error { message } => {
                assert!(
                    message.contains("only the app session"),
                    "denial must name the app-session rule, got {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn no_app_session_is_rejected() {
    let (server_state, _dir) = test_server_state();
    let sink = make_session_sink();
    handle_coordinator_chat(
        dispatch(&server_state, &sink, "session-app"),
        chat(CoordinatorChatOperation::Fetch { after_seq: 0 }),
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::Error { message } => {
            assert!(
                message.contains("only the app session"),
                "denial must name the app-session rule, got {message}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

fn subscribe(topics: &[&str]) -> FrontendRequest {
    FrontendRequest::Subscribe {
        topics: topics.iter().map(|t| (*t).to_owned()).collect(),
    }
}

#[tokio::test]
async fn app_session_may_subscribe_to_the_chat_topic() {
    use crate::app::subscriptions::handle_subscribe;
    let (server_state, _dir) = test_server_state();
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    let sink = make_session_sink();
    handle_subscribe(
        dispatch(&server_state, &sink, "session-app"),
        subscribe(&[boss_protocol::TOPIC_COORDINATOR_CHAT]),
    )
    .await;
    match sole_response(&sink).await {
        FrontendEvent::Subscribed { topics, .. } => {
            assert_eq!(topics, vec![boss_protocol::TOPIC_COORDINATOR_CHAT.to_owned()]);
        }
        other => panic!("expected Subscribed, got {other:?}"),
    }
}

#[tokio::test]
async fn non_app_and_absent_app_sessions_cannot_subscribe_to_the_chat_topic() {
    use crate::app::subscriptions::handle_subscribe;
    for register_app in [true, false] {
        for name in ["coordinator.chat", "  coordinator.chat\t"] {
            let (server_state, _dir) = test_server_state();
            if register_app {
                server_state
                    .register_app_session("session-app".into(), make_session_sink())
                    .await;
            }
            let sink = make_session_sink();
            handle_subscribe(
                dispatch(&server_state, &sink, "session-cli"),
                subscribe(&["work", name]),
            )
            .await;
            match sole_response(&sink).await {
                FrontendEvent::Error { message } => assert!(message.contains("only the app session")),
                other => panic!("expected Error, got {other:?}"),
            }
            assert!(
                server_state
                    .topic_broker
                    .subscribe("session-cli", &["work".to_owned()])
                    .await
                    == vec!["work".to_owned()],
                "a rejected subscribe must not have subscribed any topic"
            );
        }
    }
}

#[tokio::test]
async fn other_topics_are_unchanged_for_non_app_sessions() {
    use crate::app::subscriptions::handle_subscribe;
    let (server_state, _dir) = test_server_state();
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    let sink = make_session_sink();
    handle_subscribe(dispatch(&server_state, &sink, "session-cli"), subscribe(&["work"])).await;
    match sole_response(&sink).await {
        FrontendEvent::Subscribed { topics, .. } => assert_eq!(topics, vec!["work".to_owned()]),
        other => panic!("expected Subscribed, got {other:?}"),
    }
}
