use super::*;
use crate::app::coordinator_chat::handle_coordinator_chat;
use async_trait::async_trait;
use boss_engine_coordinator_chat::{ChatBackend, ChatError};
use boss_protocol::{CoordinatorChatFetchOutcome, CoordinatorChatOperation, CoordinatorChatOutcome};

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

type Answer = Box<dyn Fn(&CoordinatorChatOperation) -> Result<CoordinatorChatOutcome, ChatError> + Send + Sync>;

/// A chat service that records what it was asked and answers as told.
struct FakeBackend {
    asked: StdMutex<Vec<CoordinatorChatOperation>>,
    answer: Answer,
}

impl FakeBackend {
    fn answering(answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            asked: StdMutex::new(Vec::new()),
            answer,
        })
    }

    fn asked(&self) -> Vec<CoordinatorChatOperation> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatBackend for FakeBackend {
    async fn handle(&self, operation: CoordinatorChatOperation) -> Result<CoordinatorChatOutcome, ChatError> {
        let answer = (self.answer)(&operation);
        self.asked.lock().unwrap().push(operation);
        answer
    }
}

fn fetch_outcome() -> CoordinatorChatOutcome {
    CoordinatorChatOutcome::Fetch(Box::new(CoordinatorChatFetchOutcome {
        generation: "gen-1".to_owned(),
        items: Vec::new(),
        last_seq: 7,
        state: Default::default(),
        in_progress: None,
    }))
}

async fn app_state_with(backend: Option<Arc<FakeBackend>>) -> (Arc<ServerState>, tempfile::TempDir) {
    let (server_state, dir) = test_server_state();
    server_state
        .register_app_session("session-app".into(), make_session_sink())
        .await;
    server_state.set_coordinator_chat(backend.map(|backend| backend as Arc<dyn ChatBackend>));
    (server_state, dir)
}

async fn ask(state: &Arc<ServerState>, session_id: &str, operation: CoordinatorChatOperation) -> FrontendEvent {
    let sink = make_session_sink();
    handle_coordinator_chat(dispatch(state, &sink, session_id), chat(operation)).await;
    sole_response(&sink).await
}

#[tokio::test]
async fn an_installed_chat_service_answers_each_operation() {
    let backend = FakeBackend::answering(Box::new(|operation| {
        Ok(match operation {
            CoordinatorChatOperation::Send { client_message_id, .. } => CoordinatorChatOutcome::Sent {
                client_message_id: client_message_id.clone(),
            },
            CoordinatorChatOperation::Cancel => CoordinatorChatOutcome::cancelled(std::iter::empty()),
            CoordinatorChatOperation::AnswerPermission { request_id, .. } => {
                CoordinatorChatOutcome::PermissionAnswered {
                    request_id: request_id.clone(),
                }
            }
            CoordinatorChatOperation::Fetch { .. } => fetch_outcome(),
        })
    }));
    let (state, _dir) = app_state_with(Some(backend.clone())).await;
    let operations = operations();
    for operation in operations.clone() {
        let expected = match &operation {
            CoordinatorChatOperation::Send { client_message_id, .. } => CoordinatorChatOutcome::Sent {
                client_message_id: client_message_id.clone(),
            },
            CoordinatorChatOperation::Cancel => CoordinatorChatOutcome::cancelled(std::iter::empty()),
            CoordinatorChatOperation::AnswerPermission { request_id, .. } => {
                CoordinatorChatOutcome::PermissionAnswered {
                    request_id: request_id.clone(),
                }
            }
            CoordinatorChatOperation::Fetch { .. } => fetch_outcome(),
        };
        match ask(&state, "session-app", operation).await {
            FrontendEvent::CoordinatorChatResult { outcome } => assert_eq!(outcome, expected),
            other => panic!("expected a chat result, got {other:?}"),
        }
    }
    assert_eq!(
        backend.asked(),
        operations,
        "every operation reached the service unchanged, once"
    );
}

#[tokio::test]
async fn a_service_that_has_stopped_is_the_not_running_outcome_with_its_reason() {
    let backend = FakeBackend::answering(Box::new(|_| {
        Err(ChatError::NotRunning("the agent host went away".to_owned()))
    }));
    let (state, _dir) = app_state_with(Some(backend)).await;
    for operation in operations() {
        match ask(&state, "session-app", operation).await {
            FrontendEvent::CoordinatorChatResult {
                outcome: CoordinatorChatOutcome::BackendNotRunning { reason },
            } => assert_eq!(reason, "the agent host went away"),
            other => panic!("expected backend-not-running, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_request_the_service_refuses_is_an_error_the_app_can_show() {
    for error in [
        ChatError::Rejected("the message is empty".to_owned()),
        ChatError::Disconnected,
    ] {
        let shown = error.to_string();
        let error = std::sync::Mutex::new(Some(error));
        let backend = FakeBackend::answering(Box::new(move |_| Err(error.lock().unwrap().take().unwrap())));
        let (state, _dir) = app_state_with(Some(backend)).await;
        match ask(&state, "session-app", CoordinatorChatOperation::Cancel).await {
            FrontendEvent::Error { message } => assert_eq!(message, format!("coordinator_chat: {shown}")),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_storage_failure_is_the_not_running_outcome() {
    let backend = FakeBackend::answering(Box::new(|_| {
        Err(ChatError::Storage("the intent log could not be written".to_owned()))
    }));
    let (state, _dir) = app_state_with(Some(backend)).await;
    match ask(&state, "session-app", CoordinatorChatOperation::Cancel).await {
        FrontendEvent::CoordinatorChatResult {
            outcome: CoordinatorChatOutcome::BackendNotRunning { reason },
        } => assert_eq!(reason, "the intent log could not be written"),
        other => panic!("expected backend-not-running, got {other:?}"),
    }
}

#[tokio::test]
async fn an_installed_chat_service_is_still_closed_to_every_session_but_the_app() {
    let backend = FakeBackend::answering(Box::new(|_| Ok(CoordinatorChatOutcome::cancelled(std::iter::empty()))));
    let (state, _dir) = app_state_with(Some(backend.clone())).await;
    for operation in operations() {
        match ask(&state, "session-cli", operation).await {
            FrontendEvent::Error { message } => assert!(message.contains("only the app session"), "{message}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }
    assert!(backend.asked().is_empty(), "the service was never reached");
}

#[tokio::test]
async fn removing_the_chat_service_goes_back_to_not_running() {
    let backend = FakeBackend::answering(Box::new(|_| Ok(CoordinatorChatOutcome::cancelled(std::iter::empty()))));
    let (state, _dir) = app_state_with(Some(backend)).await;
    state.set_coordinator_chat(None);
    match ask(&state, "session-app", CoordinatorChatOperation::Cancel).await {
        FrontendEvent::CoordinatorChatResult {
            outcome: CoordinatorChatOutcome::BackendNotRunning { reason },
        } => assert_eq!(reason, "the chat backend is not running"),
        other => panic!("expected backend-not-running, got {other:?}"),
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
