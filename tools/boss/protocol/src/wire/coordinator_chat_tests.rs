//! Wire-contract tests for the coordinator-chat request, result, and topic
//! payload. Pins the snake_case tags and field names the app will decode.

use super::*;
use crate::{
    CoordinatorChatConnectionPhase, CoordinatorChatConnectionState, CoordinatorChatInProgressMessage,
    CoordinatorChatItem, CoordinatorChatItemKind, CoordinatorChatMessageDelta, CoordinatorChatMessageItem,
    CoordinatorChatMessageRole,
};

fn sample_item() -> CoordinatorChatItem {
    CoordinatorChatItem {
        id: "item-1".into(),
        seq: 1,
        updated_seq: 1,
        ts_ms: 1_795_000_000_001,
        kind: CoordinatorChatItemKind::Message(CoordinatorChatMessageItem {
            role: CoordinatorChatMessageRole::Operator,
            text: "hello".into(),
            client_message_id: Some("c1".into()),
            message_id: None,
            sender: None,
        }),
    }
}

fn request_json(operation: CoordinatorChatOperation) -> serde_json::Value {
    serde_json::to_value(FrontendRequest::CoordinatorChat { operation }).unwrap()
}

#[test]
fn send_cancel_answer_and_fetch_round_trip() {
    let send = request_json(CoordinatorChatOperation::Send {
        text: "hi".into(),
        client_message_id: "c1".into(),
        attachments: vec![CoordinatorChatSendAttachment {
            path: "/tmp/shot.png".into(),
        }],
    });
    assert_eq!(
        send,
        serde_json::json!({
            "type": "coordinator_chat",
            "op": "send",
            "text": "hi",
            "client_message_id": "c1",
            "attachments": [{"path": "/tmp/shot.png"}],
        })
    );
    assert!(matches!(
        serde_json::from_value::<FrontendRequest>(send).unwrap(),
        FrontendRequest::CoordinatorChat {
            operation: CoordinatorChatOperation::Send { client_message_id, .. },
        } if client_message_id == "c1"
    ));

    let send_no_attachments = request_json(CoordinatorChatOperation::Send {
        text: "hi".into(),
        client_message_id: "c1".into(),
        attachments: vec![],
    });
    assert!(send_no_attachments.get("attachments").is_none());

    let cancel = request_json(CoordinatorChatOperation::Cancel);
    assert_eq!(cancel, serde_json::json!({"type": "coordinator_chat", "op": "cancel"}));
    assert!(matches!(
        serde_json::from_value::<FrontendRequest>(cancel).unwrap(),
        FrontendRequest::CoordinatorChat {
            operation: CoordinatorChatOperation::Cancel,
        }
    ));

    let answer = request_json(CoordinatorChatOperation::AnswerPermission {
        request_id: "0".into(),
        option_id: "allow-once".into(),
    });
    assert_eq!(
        answer,
        serde_json::json!({
            "type": "coordinator_chat",
            "op": "answer_permission",
            "request_id": "0",
            "option_id": "allow-once",
        })
    );

    let fetch = request_json(CoordinatorChatOperation::Fetch { after_seq: 12 });
    assert_eq!(
        fetch,
        serde_json::json!({
            "type": "coordinator_chat",
            "op": "fetch",
            "after_seq": 12,
        })
    );
    assert!(matches!(
        serde_json::from_value::<FrontendRequest>(fetch).unwrap(),
        FrontendRequest::CoordinatorChat {
            operation: CoordinatorChatOperation::Fetch { after_seq: 12 },
        }
    ));
}

#[test]
fn result_backend_not_running_wire_shape_is_pinned() {
    let event = FrontendEvent::CoordinatorChatResult {
        outcome: CoordinatorChatOutcome::BackendNotRunning {
            reason: "the chat backend is not running".into(),
        },
    };
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "type": "coordinator_chat_result",
            "outcome": {
                "type": "backend_not_running",
                "reason": "the chat backend is not running",
            },
        })
    );
    assert!(matches!(
        serde_json::from_value::<FrontendEvent>(value).unwrap(),
        FrontendEvent::CoordinatorChatResult {
            outcome: CoordinatorChatOutcome::BackendNotRunning { .. },
        }
    ));
}

#[test]
fn result_fetch_and_sent_round_trip() {
    let fetch = FrontendEvent::CoordinatorChatResult {
        outcome: CoordinatorChatOutcome::Fetch(Box::new(CoordinatorChatFetchOutcome {
            generation: "gen_1".into(),
            items: vec![sample_item()],
            last_seq: 1,
            state: CoordinatorChatConnectionState::default(),
            in_progress: Some(CoordinatorChatInProgressMessage {
                item_id: "item-2".into(),
                started_seq: 2,
                updated_seq: 2,
                role: CoordinatorChatMessageRole::Coordinator,
                text: "partial".into(),
            }),
        })),
    };
    let value = serde_json::to_value(&fetch).unwrap();
    assert_eq!(value["type"], "coordinator_chat_result");
    assert_eq!(value["outcome"]["type"], "fetch");
    assert_eq!(value["outcome"]["generation"], "gen_1");
    assert_eq!(value["outcome"]["last_seq"], 1);
    assert_eq!(value["outcome"]["items"][0]["id"], "item-1");
    let decoded: FrontendEvent = serde_json::from_value(value).unwrap();
    assert_eq!(
        serde_json::to_value(&decoded).unwrap(),
        serde_json::to_value(&fetch).unwrap()
    );

    let sent = serde_json::to_value(FrontendEvent::CoordinatorChatResult {
        outcome: CoordinatorChatOutcome::Sent {
            client_message_id: "c1".into(),
        },
    })
    .unwrap();
    assert_eq!(sent["outcome"]["type"], "sent");
    assert_eq!(sent["outcome"]["client_message_id"], "c1");
}

#[test]
fn topic_update_item_delta_and_state_round_trip() {
    let item = TopicEventPayload::CoordinatorChat {
        generation: "gen_1".into(),
        update: CoordinatorChatTopicUpdate::Item { item: sample_item() },
    };
    let value = serde_json::to_value(&item).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "type": "coordinator_chat",
            "generation": "gen_1",
            "update": {
                "type": "item",
                "item": {
                    "id": "item-1",
                    "seq": 1,
                    "updated_seq": 1,
                    "ts_ms": 1_795_000_000_001u64,
                    "kind": {
                        "type": "message",
                        "role": "operator",
                        "text": "hello",
                        "client_message_id": "c1",
                    },
                },
            },
        })
    );

    let delta = TopicEventPayload::CoordinatorChat {
        generation: "gen_1".into(),
        update: CoordinatorChatTopicUpdate::Delta {
            delta: CoordinatorChatMessageDelta {
                item_id: "item-2".into(),
                seq: 4,
                offset: 3,
                text: "lo".into(),
            },
        },
    };
    let value = serde_json::to_value(&delta).unwrap();
    assert_eq!(value["update"]["type"], "delta");
    assert_eq!(value["update"]["delta"]["offset"], 3);

    let state = TopicEventPayload::CoordinatorChat {
        generation: "gen_1".into(),
        update: CoordinatorChatTopicUpdate::State {
            state: CoordinatorChatConnectionState::builder()
                .phase(CoordinatorChatConnectionPhase::Failed {
                    reason: "the chat backend could not start".into(),
                })
                .build(),
        },
    };
    let value = serde_json::to_value(&state).unwrap();
    assert_eq!(value["type"], "coordinator_chat");
    assert_eq!(value["generation"], "gen_1");
    assert_eq!(value["update"]["type"], "state");
    assert_eq!(value["update"]["state"]["phase"]["phase"], "failed");
    assert_eq!(
        value["update"]["state"]["phase"]["reason"],
        "the chat backend could not start"
    );
    assert!(matches!(
        serde_json::from_value::<TopicEventPayload>(value).unwrap(),
        TopicEventPayload::CoordinatorChat {
            update: CoordinatorChatTopicUpdate::State { state },
            ..
        } if matches!(
            state.phase,
            CoordinatorChatConnectionPhase::Failed { ref reason }
                if reason == "the chat backend could not start"
        )
    ));
}
