use boss_acp_client::schema::RequestId;
use boss_agent_host::journal::{Direction, WireRecord, read_records};
use boss_protocol::{
    CoordinatorChatAuthState, CoordinatorChatConnectionPhase, CoordinatorChatItemKind, CoordinatorChatMessageRole,
    CoordinatorChatNoticeKind, CoordinatorChatPermissionResolution, CoordinatorChatToolStatus,
    CoordinatorChatTurnOrigin,
};
use serde_json::json;

use crate::intent::{IntentRecord, IntentRecordKind};
use crate::queue::PromptOrigin;
use crate::redact::{CredentialRedactor, REDACTED_PLACEHOLDER};
use crate::reducer::{
    ConversationReducer, DEFAULT_TOOL_TEXT_LIMIT, replay_reader, replay_records, replay_with_intent_log,
};

const BG_FIXTURE: &str = include_str!("../testdata/background_agent_turn.jsonl");
const PERMISSION_CANCEL_FIXTURE: &str = include_str!("../testdata/permission_and_cancel.jsonl");
const TRUNCATED_FIXTURE: &str = include_str!("../testdata/truncated_mid_turn.jsonl");

const TEST_SECRET: &str = "handoff-write-secret-0123456789abcdef";

fn record(seq: u64, dir: Direction, payload: serde_json::Value) -> WireRecord {
    WireRecord {
        seq,
        ts_ms: 1_795_000_000_000 + seq,
        dir,
        line: payload.to_string(),
        invalid: None,
    }
}

#[test]
fn replays_background_agent_completion_agent_turn_compaction_and_account_state() {
    let records = read_records(BG_FIXTURE.as_bytes()).expect("fixture must parse cleanly");
    assert_eq!(records.len(), 49);

    let first = replay_records(&records, CredentialRedactor::none());
    let second = replay_records(&records, CredentialRedactor::none());

    // Deterministic under replay.
    assert_eq!(first.state(), second.state());
    assert_eq!(first.items(), second.items());
    assert_eq!(first.in_progress_message(), None);

    let state = first.state();
    assert_eq!(state.phase, CoordinatorChatConnectionPhase::Idle);
    assert_eq!(state.auth_state, CoordinatorChatAuthState::Authenticated);
    assert_eq!(
        state.agent_name.as_deref(),
        Some("@agentclientprotocol/claude-agent-acp")
    );
    assert_eq!(state.agent_version.as_deref(), Some("0.110.0"));
    assert!(state.supports_background_subagents);
    assert!(state.supports_async_tasks);
    assert_eq!(
        state.session_id.as_deref(),
        Some("8f841612-5203-404c-a918-23204324e257")
    );
    assert_eq!(state.model.as_deref(), Some("claude-sonnet-5-5"));
    assert_eq!(state.mode.as_deref(), Some("bypassPermissions"));
    assert_eq!(state.last_seq, 49);

    // Account notifications become a state only, never their content.
    let state_json = serde_json::to_string(state).unwrap();
    let items_json = serde_json::to_string(first.items()).unwrap();
    for forbidden in ["redacted-user@example.invalid", "Example Organization", "Example Plan"] {
        assert!(
            !state_json.contains(forbidden),
            "state leaked account content: {state_json}"
        );
        assert!(
            !items_json.contains(forbidden),
            "items leaked account content: {items_json}"
        );
    }

    // Prompts are not echoed on the wire, so no operator message item was
    // fabricated from `session/prompt`.
    assert!(
        !first.items().iter().any(|item| matches!(
            &item.kind,
            CoordinatorChatItemKind::Message(msg) if msg.role == CoordinatorChatMessageRole::Operator
        )),
        "wire replay alone must not fabricate operator messages"
    );

    // Items are strictly ordered by creation seq.
    let seqs: Vec<u64> = first.items().iter().map(|item| item.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] <= w[1]), "items out of order: {seqs:?}");

    // Verify background-agent child tool call kept its parentToolUseId and
    // preserved fenced markdown output verbatim.
    let child_tool = first
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::ToolCall(tool) if tool.tool_call_id == "toolu_01W1mchxf3S8S3xo2kngiL1L" => {
                Some(tool)
            }
            _ => None,
        })
        .expect("background child tool call must exist");
    assert_eq!(
        child_tool.parent_tool_call_id.as_deref(),
        Some("toolu_01Gn6NL2R1tvsumQYQx4uKrk")
    );
    assert_eq!(child_tool.status, CoordinatorChatToolStatus::Completed);
    assert_eq!(
        child_tool.output.as_deref(),
        Some("```console\nBACKGROUND-DONE-58\n```")
    );

    // Verify phase stayed Idle while the background agent's own tool call ran
    // outside any turn, then moved to Working on `async_task_spawned` (`turn`),
    // and returned to Idle on `async_task_state_update` (`completed`).
    let mut step_reducer = ConversationReducer::new(CredentialRedactor::none());
    for rec in &records {
        step_reducer.apply_record(rec);
        match rec.seq {
            30..=32 => assert_eq!(
                step_reducer.state().phase,
                CoordinatorChatConnectionPhase::Idle,
                "seq {} must not open a turn",
                rec.seq
            ),
            33..=39 => assert_eq!(
                step_reducer.state().phase,
                CoordinatorChatConnectionPhase::Working,
                "seq {} is inside agent-started turn",
                rec.seq
            ),
            40..=42 => assert_eq!(
                step_reducer.state().phase,
                CoordinatorChatConnectionPhase::Idle,
                "seq {} is after agent-started turn closed",
                rec.seq
            ),
            _ => {}
        }
    }

    // Verify agent-started turn produced a coordinator message and a turn
    // boundary whose origin was populated by the trailing `usage_update`.
    let agent_message = first
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::Message(msg)
                if msg.message_id.as_deref() == Some("msg_011ChRrrcRSsV7f21Y7hKHqh") =>
            {
                Some(msg)
            }
            _ => None,
        })
        .expect("agent-started turn message must exist");
    assert_eq!(agent_message.role, CoordinatorChatMessageRole::Coordinator);
    assert!(
        agent_message.text.contains("BACKGROUND-DONE-58"),
        "{}",
        agent_message.text
    );

    let agent_turn_boundary = first
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::TurnBoundary(boundary) if boundary.agent_initiated => Some((item, boundary)),
            _ => None,
        })
        .expect("agent-started turn boundary must exist");
    assert_eq!(agent_turn_boundary.1.stop_reason.as_deref(), Some("completed"));
    assert_eq!(
        agent_turn_boundary.1.origin,
        Some(CoordinatorChatTurnOrigin::TaskNotification)
    );
    assert_eq!(agent_turn_boundary.0.seq, 40);
    assert_eq!(agent_turn_boundary.0.updated_seq, 41);

    // Compaction tool call (`_meta.jetbrains.air.contextCompaction`) became a
    // Compaction notice, not a ToolCall.
    assert!(
        !first.items().iter().any(|item| matches!(
            &item.kind,
            CoordinatorChatItemKind::ToolCall(tool) if tool.tool_call_id == "5cda7d36-2bf2-4b3b-af17-32eed8ddc3c0"
        )),
        "compact tool call must become a compaction notice, not a generic tool row"
    );
    let compaction = first
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::Notice(notice) if notice.notice_kind == CoordinatorChatNoticeKind::Compaction => {
                Some(notice)
            }
            _ => None,
        })
        .expect("compaction notice must exist");
    assert_eq!(compaction.trigger.as_deref(), Some("manual"));
    assert_eq!(compaction.pre_tokens, Some(25909));
    assert_eq!(compaction.post_tokens, Some(7189));
}

#[test]
fn replays_permission_allow_cancel_and_agent_withdrawal() {
    let records = read_records(PERMISSION_CANCEL_FIXTURE.as_bytes()).expect("fixture must parse cleanly");
    assert_eq!(records.len(), 52);
    let mut reducer = ConversationReducer::new(CredentialRedactor::none());

    for rec in &records {
        reducer.apply_record(rec);
        match rec.seq {
            22 | 37 | 49 => assert_eq!(
                reducer.state().phase,
                CoordinatorChatConnectionPhase::WaitingForPermission,
                "seq {} should wait for permission",
                rec.seq
            ),
            24 | 39 | 51 => assert_eq!(
                reducer.state().phase,
                CoordinatorChatConnectionPhase::Working,
                "seq {} should resume working after permission resolved/withdrawn",
                rec.seq
            ),
            40 => assert!(reducer.state().cancel_requested),
            43 => {
                assert_eq!(reducer.state().phase, CoordinatorChatConnectionPhase::Idle);
                assert!(!reducer.state().cancel_requested);
            }
            _ => {}
        }
    }

    // Child crash `-32603` moves phase to Failed.
    assert!(
        matches!(reducer.state().phase, CoordinatorChatConnectionPhase::Failed { .. }),
        "expected Failed phase after child crash, got {:?}",
        reducer.state().phase
    );

    let permissions: Vec<_> = reducer
        .items()
        .iter()
        .filter_map(|item| match &item.kind {
            CoordinatorChatItemKind::PermissionRequest(perm) => Some(perm),
            _ => None,
        })
        .collect();
    assert_eq!(permissions.len(), 3);

    // Request 0: allowed with option_name resolved from the agent's options.
    assert_eq!(permissions[0].request_id, "0");
    assert_eq!(
        permissions[0].resolution,
        CoordinatorChatPermissionResolution::Selected {
            option_id: "allow-once".to_owned(),
            option_name: Some("Yes".to_owned()),
        }
    );

    // Request 1: cancelled by client before `session/cancel` (did not collide
    // with client's request id 1 `initialize`).
    assert_eq!(permissions[1].request_id, "1");
    assert_eq!(
        permissions[1].resolution,
        CoordinatorChatPermissionResolution::Cancelled { withdrawn: false }
    );

    // Request 2: withdrawn by agent `$/cancel_request`, rendered as cancelled.
    assert_eq!(permissions[2].request_id, "2");
    assert_eq!(
        permissions[2].resolution,
        CoordinatorChatPermissionResolution::Cancelled { withdrawn: true }
    );

    // Cancelled tool call carries status Failed.
    let cancelled_tool = reducer
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::ToolCall(tool) if tool.tool_call_id == "toolu_01CcdsvjJQd4vqfb5YPj3CzQ" => {
                Some(tool)
            }
            _ => None,
        })
        .expect("cancelled tool must exist");
    assert_eq!(cancelled_tool.status, CoordinatorChatToolStatus::Failed);

    // Turn 2 boundary recorded `cancelled`.
    let turn_boundaries: Vec<_> = reducer
        .items()
        .iter()
        .filter_map(|item| match &item.kind {
            CoordinatorChatItemKind::TurnBoundary(boundary) => Some(boundary),
            _ => None,
        })
        .collect();
    assert_eq!(turn_boundaries.len(), 3);
    assert_eq!(turn_boundaries[0].stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(turn_boundaries[0].origin, Some(CoordinatorChatTurnOrigin::Human));
    assert_eq!(turn_boundaries[1].stop_reason.as_deref(), Some("cancelled"));
    assert!(turn_boundaries[2].error.is_some());
}

#[test]
fn replays_truncated_journal_tolerating_torn_final_line_and_preserving_in_progress_message() {
    let reducer = replay_reader(TRUNCATED_FIXTURE.as_bytes(), CredentialRedactor::none())
        .expect("truncated final line must be ignored cleanly");

    assert_eq!(reducer.state().last_seq, 18);
    assert_eq!(reducer.state().phase, CoordinatorChatConnectionPhase::Working);

    let in_progress = reducer
        .in_progress_message()
        .expect("partial streamed answer must be exposed in snapshot");
    assert_eq!(in_progress.item_id, "msg-17");
    assert_eq!(in_progress.started_seq, 17);
    assert_eq!(in_progress.updated_seq, 18);
    assert_eq!(in_progress.text, "PARTIAL_TURN_MARKER one");

    // A corrupt record in the middle is still a hard error.
    let corrupt_middle = format!("not-json\n{TRUNCATED_FIXTURE}");
    assert!(replay_reader(corrupt_middle.as_bytes(), CredentialRedactor::none()).is_err());
}

#[test]
fn redacts_credential_in_tool_input_output_permission_and_intent_prompt() {
    let mut reducer = ConversationReducer::new(CredentialRedactor::new(TEST_SECRET));
    reducer.apply_intent_record(&IntentRecord {
        seq: 1,
        ts_ms: 1_795_000_000_000,
        kind: IntentRecordKind::Queued {
            intent_id: 1,
            origin: PromptOrigin::Engine,
            text: format!("Private handoff write authorization: {TEST_SECRET}"),
            client_message_id: None,
            images: Vec::new(),
        },
    });
    reducer.apply_intent_record(&IntentRecord {
        seq: 2,
        ts_ms: 1_795_000_000_001,
        kind: IntentRecordKind::Sent {
            intent_id: 1,
            request_id: RequestId::Number(1),
        },
    });

    let records = [
        record(
            1,
            Direction::ToAgent,
            json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
        ),
        record(
            2,
            Direction::FromAgent,
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s",
                    "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": "toolu_handoff",
                        "title": format!("boss handoff write --authorization {TEST_SECRET} -"),
                        "kind": "execute",
                        "status": "in_progress",
                        "rawInput": {"command": format!("boss handoff write --authorization {TEST_SECRET} -")},
                        "_meta": {"claudeCode": {"toolName": "Bash"}}
                    }
                }
            }),
        ),
        record(
            3,
            Direction::FromAgent,
            json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "session/request_permission",
                "params": {
                    "sessionId": "s",
                    "toolCall": {
                        "toolCallId": "toolu_handoff",
                        "title": format!("boss handoff write --authorization {TEST_SECRET} -"),
                        "kind": "execute",
                        "rawInput": {"command": format!("boss handoff write --authorization {TEST_SECRET} -")}
                    },
                    "options": [{"optionId": "allow", "name": "Yes", "kind": "allow_once"}]
                }
            }),
        ),
        record(
            4,
            Direction::FromAgent,
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s",
                    "update": {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": "toolu_handoff",
                        "status": "completed",
                        "content": [{"type": "content", "content": {"type": "text", "text": format!("```text\nused {TEST_SECRET}\n```")}}]
                    }
                }
            }),
        ),
    ];

    for rec in &records {
        reducer.apply_record(rec);
    }

    let serialized = serde_json::to_string(reducer.items()).unwrap();
    assert!(!serialized.contains(TEST_SECRET), "credential leaked: {serialized}");
    assert!(serialized.contains(REDACTED_PLACEHOLDER));

    let tool = reducer
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::ToolCall(tool) => Some(tool),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        tool.title,
        format!("boss handoff write --authorization {REDACTED_PLACEHOLDER} -")
    );
    assert_eq!(
        tool.input.as_deref(),
        Some(format!(r#"{{"command":"boss handoff write --authorization {REDACTED_PLACEHOLDER} -"}}"#).as_str())
    );
    assert_eq!(
        tool.output.as_deref(),
        Some(format!("```text\nused {REDACTED_PLACEHOLDER}\n```").as_str())
    );
}

#[test]
fn redacts_credential_split_across_two_streamed_chunks_without_leaking_prefix() {
    let mut reducer = ConversationReducer::new(CredentialRedactor::new(TEST_SECRET));
    let (first_half, second_half) = TEST_SECRET.split_at(TEST_SECRET.len() / 2);

    reducer.apply_record(&record(
        1,
        Direction::ToAgent,
        json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
    ));

    let delta1 = reducer.apply_record(&record(
        2,
        Direction::FromAgent,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "m1",
                    "content": {"type": "text", "text": format!("Done writing handoff with token {first_half}")}
                }
            }
        }),
    ));
    let snap1 = reducer.in_progress_message().expect("message in progress");
    assert!(
        !snap1.text.contains("handoff-write"),
        "snapshot leaked prefix: {}",
        snap1.text
    );
    if let Some(md) = &delta1.message_delta {
        assert!(!md.text.contains("handoff-write"), "delta1 leaked prefix: {}", md.text);
        assert_eq!(md.offset, 0);
        assert_eq!(md.text, snap1.text);
    }

    let delta2 = reducer.apply_record(&record(
        3,
        Direction::FromAgent,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "m1",
                    "content": {"type": "text", "text": format!("{second_half} cleanly.")}
                }
            }
        }),
    ));
    let snap2 = reducer.in_progress_message().expect("message still in progress");
    assert!(!snap2.text.contains(TEST_SECRET));
    assert!(!snap2.text.contains(first_half));
    assert!(snap2.text.contains(REDACTED_PLACEHOLDER), "snap2 = {}", snap2.text);
    let md2 = delta2.message_delta.expect("chunk 2 must emit redacted delta");
    assert_eq!(md2.offset, snap1.text.len());
    assert_eq!(format!("{}{}", snap1.text, md2.text), snap2.text);

    // Finalizing the turn flushes the remaining safe tail into the completed item.
    reducer.apply_record(&record(
        4,
        Direction::FromAgent,
        json!({"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}),
    ));
    assert!(reducer.in_progress_message().is_none());

    let finalized = reducer
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::Message(msg) => Some(msg),
            _ => None,
        })
        .expect("finalized message must exist");
    assert_eq!(
        finalized.text,
        format!("Done writing handoff with token {REDACTED_PLACEHOLDER} cleanly.")
    );
}

#[test]
fn redacts_credential_straddling_tool_truncation_boundary() {
    // Limit falls right in the middle of the secret if truncation ran first.
    let mut reducer = ConversationReducer::with_tool_text_limit(CredentialRedactor::new(TEST_SECRET), 18);
    reducer.apply_record(&record(
        1,
        Direction::FromAgent,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s",
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "toolu_trunc",
                    "title": "cmd",
                    "rawInput": format!("prefix1234 {TEST_SECRET} trailing-text")
                }
            }
        }),
    ));
    let tool = reducer
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::ToolCall(tool) => Some(tool),
            _ => None,
        })
        .unwrap();
    let input = tool.input.as_deref().unwrap();
    assert!(
        !input.contains("handoff-write"),
        "partial secret leaked at cut: {input}"
    );
    assert!(input.contains("truncated:"), "expected truncation marker: {input}");
}

#[test]
fn peer_origin_and_thought_chunks_and_invalid_records_are_handled() {
    let mut reducer = ConversationReducer::new(CredentialRedactor::new(TEST_SECRET));
    let records = [
        record(
            1,
            Direction::FromAgent,
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s",
                    "update": {
                        "sessionUpdate": "async_task_spawned",
                        "asyncTaskId": "autonomous-turn-peer",
                        "taskType": "turn",
                        "name": "Claude is working"
                    }
                }
            }),
        ),
        record(
            2,
            Direction::FromAgent,
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s",
                    "update": {
                        "sessionUpdate": "agent_thought_chunk",
                        "content": {"type": "text", "text": format!("thinking with {TEST_SECRET}")}
                    }
                }
            }),
        ),
        WireRecord {
            seq: 3,
            ts_ms: 1_795_000_000_003,
            dir: Direction::FromAgent,
            line: "invalid preview".to_owned(),
            invalid: Some("invalid_utf8".to_owned()),
        },
        record(
            4,
            Direction::FromAgent,
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s",
                    "update": {
                        "sessionUpdate": "async_task_state_update",
                        "asyncTaskId": "autonomous-turn-peer",
                        "state": "completed"
                    }
                }
            }),
        ),
        record(
            5,
            Direction::FromAgent,
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s",
                    "update": {
                        "sessionUpdate": "usage_update",
                        "used": 1234,
                        "size": 1000000,
                        "_meta": {
                            "_claude/origin": {
                                "kind": "peer",
                                "name": "other-session",
                                "verifiedLocal": true,
                                "body": "ping"
                            }
                        }
                    }
                }
            }),
        ),
    ];

    for rec in &records {
        reducer.apply_record(rec);
    }

    let thought = reducer
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::Thought(thought) => Some(thought),
            _ => None,
        })
        .expect("thought item must be flushed");
    assert_eq!(thought.text, format!("thinking with {REDACTED_PLACEHOLDER}"));

    let boundary = reducer
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::TurnBoundary(boundary) => Some(boundary),
            _ => None,
        })
        .expect("turn boundary must exist");
    assert!(boundary.agent_initiated);
    assert_eq!(boundary.origin, Some(CoordinatorChatTurnOrigin::Peer));
}

fn update_record(seq: u64, update: serde_json::Value) -> WireRecord {
    record(
        seq,
        Direction::FromAgent,
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":update}}),
    )
}

fn chunk(seq: u64, text: &str) -> WireRecord {
    update_record(
        seq,
        json!({"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"type":"text","text":text}}),
    )
}

#[test]
fn credential_split_around_background_child_tool_call_never_leaks() {
    let mut reducer = ConversationReducer::new(CredentialRedactor::new(TEST_SECRET));
    let (first_half, second_half) = TEST_SECRET.split_at(TEST_SECRET.len() / 2);
    let records = [
        record(
            1,
            Direction::ToAgent,
            json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
        ),
        chunk(2, &format!("token {first_half}")),
        update_record(
            3,
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "toolu_child",
                "title": "child work",
                "kind": "other",
                "status": "in_progress",
                "_meta": {"claudeCode": {"toolName": "Read", "parentToolUseId": "toolu_parent"}}
            }),
        ),
        chunk(4, &format!("{second_half} done")),
    ];
    for record in &records {
        let delta = reducer.apply_record(record);
        let snapshot =
            serde_json::to_string(&(reducer.items(), reducer.in_progress_message(), &delta.message_delta)).unwrap();
        assert!(!snapshot.contains(first_half), "intermediate leak: {snapshot}");
    }
    reducer.apply_record(&record(
        5,
        Direction::FromAgent,
        json!({"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}),
    ));
    let seqs: Vec<u64> = reducer.items().iter().map(|item| item.seq).collect();
    assert_eq!(seqs, vec![2, 3, 5], "items must remain ordered by creation seq");
    let messages: Vec<_> = reducer
        .items()
        .iter()
        .filter_map(|item| match &item.kind {
            CoordinatorChatItemKind::Message(message) => Some(message.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(messages, vec![format!("token {REDACTED_PLACEHOLDER} done")]);

    // A later update to the child tool must still target the shifted tool index.
    reducer.apply_record(&update_record(
        6,
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "toolu_child",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "child ok"}}]
        }),
    ));
    let child = reducer
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::ToolCall(tool) if tool.tool_call_id == "toolu_child" => Some(tool),
            _ => None,
        })
        .expect("child tool must exist");
    assert_eq!(child.status, CoordinatorChatToolStatus::Completed);
    assert_eq!(child.output.as_deref(), Some("child ok"));
}

#[test]
fn credential_split_around_background_child_orphan_update_never_leaks() {
    let mut reducer = ConversationReducer::new(CredentialRedactor::new(TEST_SECRET));
    let (first_half, second_half) = TEST_SECRET.split_at(TEST_SECRET.len() / 2);
    let records = [
        record(
            1,
            Direction::ToAgent,
            json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
        ),
        chunk(2, &format!("token {first_half}")),
        update_record(
            3,
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "toolu_child_orphan",
                "title": "orphan child update",
                "kind": "execute",
                "status": "in_progress",
                "_meta": {"claudeCode": {"toolName": "Bash", "parentToolUseId": "toolu_parent"}}
            }),
        ),
        chunk(4, &format!("{second_half} done")),
    ];
    for rec in &records {
        let delta = reducer.apply_record(rec);
        let snapshot =
            serde_json::to_string(&(reducer.items(), reducer.in_progress_message(), &delta.message_delta)).unwrap();
        assert!(!snapshot.contains(first_half), "intermediate leak: {snapshot}");
    }
    reducer.apply_record(&record(
        5,
        Direction::FromAgent,
        json!({"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}),
    ));

    let seqs: Vec<u64> = reducer.items().iter().map(|item| item.seq).collect();
    assert_eq!(seqs, vec![2, 3, 5], "items must remain ordered by creation seq");
    let messages: Vec<_> = reducer
        .items()
        .iter()
        .filter_map(|item| match &item.kind {
            CoordinatorChatItemKind::Message(message) => Some(message.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(messages, vec![format!("token {REDACTED_PLACEHOLDER} done")]);

    // Later update to the synthesized orphan tool still hits the shifted index.
    reducer.apply_record(&update_record(
        6,
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "toolu_child_orphan",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "orphan ok"}}]
        }),
    ));
    let child = reducer
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::ToolCall(tool) if tool.tool_call_id == "toolu_child_orphan" => Some(tool),
            _ => None,
        })
        .expect("orphan child tool must exist");
    assert_eq!(child.status, CoordinatorChatToolStatus::Completed);
    assert_eq!(child.output.as_deref(), Some("orphan ok"));
}

#[test]
fn credential_split_around_background_child_permission_request_never_leaks() {
    let mut reducer = ConversationReducer::new(CredentialRedactor::new(TEST_SECRET));
    let (first_half, second_half) = TEST_SECRET.split_at(TEST_SECRET.len() / 2);
    let records = [
        record(
            1,
            Direction::ToAgent,
            json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
        ),
        chunk(2, &format!("token {first_half}")),
        record(
            3,
            Direction::FromAgent,
            json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "session/request_permission",
                "params": {
                    "sessionId": "s",
                    "toolCall": {
                        "toolCallId": "toolu_child_perm",
                        "title": "child command",
                        "kind": "execute",
                        "_meta": {"claudeCode": {"parentToolUseId": "toolu_parent"}}
                    },
                    "options": [{"optionId": "allow-once", "name": "Yes", "kind": "allow_once"}]
                }
            }),
        ),
        chunk(4, &format!("{second_half} done")),
    ];
    for rec in &records {
        let delta = reducer.apply_record(rec);
        let snapshot =
            serde_json::to_string(&(reducer.items(), reducer.in_progress_message(), &delta.message_delta)).unwrap();
        assert!(!snapshot.contains(first_half), "intermediate leak: {snapshot}");
    }
    reducer.apply_record(&record(
        5,
        Direction::FromAgent,
        json!({"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}),
    ));

    let seqs: Vec<u64> = reducer.items().iter().map(|item| item.seq).collect();
    assert_eq!(seqs, vec![2, 3, 5], "items must remain ordered by creation seq");
    let messages: Vec<_> = reducer
        .items()
        .iter()
        .filter_map(|item| match &item.kind {
            CoordinatorChatItemKind::Message(message) => Some(message.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(messages, vec![format!("token {REDACTED_PLACEHOLDER} done")]);

    // Resolving the child permission request after the message flushed still
    // updates the shifted permission index.
    reducer.apply_record(&record(
        6,
        Direction::ToAgent,
        json!({"jsonrpc":"2.0","id":9,"result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}),
    ));
    let perm = reducer
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::PermissionRequest(perm) if perm.request_id == "9" => Some(perm),
            _ => None,
        })
        .expect("child permission request must exist");
    assert_eq!(
        perm.resolution,
        CoordinatorChatPermissionResolution::Selected {
            option_id: "allow-once".to_owned(),
            option_name: Some("Yes".to_owned()),
        }
    );
}

#[test]
fn replay_with_intent_log_matches_live_interleaved_reduction() {
    let records = [
        record(
            2,
            Direction::ToAgent,
            json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
        ),
        chunk(3, "answer"),
        record(
            4,
            Direction::FromAgent,
            json!({"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}),
        ),
    ];
    let intents = [
        IntentRecord {
            seq: 1,
            ts_ms: 1_795_000_000_000,
            kind: IntentRecordKind::Queued {
                intent_id: 1,
                origin: PromptOrigin::Operator,
                text: "hello".to_owned(),
                client_message_id: Some("cm-1".to_owned()),
                images: Vec::new(),
            },
        },
        IntentRecord {
            seq: 2,
            ts_ms: 1_795_000_000_001,
            kind: IntentRecordKind::Sent {
                intent_id: 1,
                request_id: RequestId::Number(1),
            },
        },
    ];
    let mut live = ConversationReducer::new(CredentialRedactor::none());
    for intent in &intents {
        live.apply_intent_record(intent);
    }
    for record in &records {
        live.apply_record(record);
    }
    let replayed = replay_with_intent_log(&records, &intents, CredentialRedactor::none(), DEFAULT_TOOL_TEXT_LIMIT);
    assert_eq!(replayed.items(), live.items());
    assert_eq!(replayed.state(), live.state());
    assert_eq!(replayed.queue(), live.queue());
    assert!(matches!(
        &replayed.items()[0].kind,
        CoordinatorChatItemKind::Message(m) if m.role == CoordinatorChatMessageRole::Operator && m.text == "hello"
    ));
}

#[test]
fn cancel_requested_clears_when_agent_started_turn_closes() {
    let mut reducer = ConversationReducer::new(CredentialRedactor::none());
    let spawn = |seq, id: &str| {
        update_record(
            seq,
            json!({"sessionUpdate":"async_task_spawned","asyncTaskId":id,"taskType":"turn"}),
        )
    };
    let stop = |seq, id: &str| {
        update_record(
            seq,
            json!({"sessionUpdate":"async_task_state_update","asyncTaskId":id,"state":"stopped"}),
        )
    };
    reducer.apply_record(&spawn(1, "t1"));
    reducer.apply_record(&record(
        2,
        Direction::ToAgent,
        json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}),
    ));
    assert!(reducer.state().cancel_requested);
    reducer.apply_record(&stop(3, "t1"));
    assert!(!reducer.state().cancel_requested);
    reducer.apply_record(&spawn(4, "t2"));
    assert!(!reducer.state().cancel_requested);
}

#[test]
fn truncated_fenced_tool_output_closes_the_fence_before_the_notice() {
    let text = format!("```text\n{}\n```", "x".repeat(200));
    let bounded = crate::reducer::bound_text(&text, 50);
    assert!(bounded.contains("\n```\n… truncated"), "{bounded}");
}

mod restart_and_snapshot_tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Instant;

    use boss_acp_client::schema::RequestId;
    use boss_agent_host::journal::{Direction, WireRecord, read_records};
    use boss_protocol::{
        CoordinatorChatConnectionPhase, CoordinatorChatItemKind, CoordinatorChatMessageRole, CoordinatorChatTurnOrigin,
    };
    use serde_json::json;

    use crate::intent::{IntentLog, IntentRecord, IntentRecordKind, WithdrawReason};
    use crate::queue::{PromptImageRef, PromptOrigin};
    use crate::redact::{CredentialRedactor, REDACTED_PLACEHOLDER};
    use crate::reducer::{ConversationReducer, DEFAULT_TOOL_TEXT_LIMIT, replay_with_intent_log};
    use crate::snapshot::{
        SNAPSHOT_SCHEMA_VERSION, SnapshotLoad, SnapshotLoadStatus, UnsentDispatchPolicy, load_snapshot,
        load_snapshot_bytes, recover_from_files, recover_from_slices, recover_from_snapshot_and_tail, save_snapshot,
    };

    const RESTART_FIXTURE: &str = include_str!("../testdata/restart_mid_turn_with_queue.jsonl");
    const TEST_SECRET: &str = "handoff-write-secret-0123456789abcdef";

    fn wire(seq: u64, dir: Direction, payload: serde_json::Value) -> WireRecord {
        WireRecord {
            seq,
            ts_ms: 1_795_720_000_000 + seq * 10,
            dir,
            line: payload.to_string(),
            invalid: None,
        }
    }

    fn write_wire_file(path: &std::path::Path, records: &[WireRecord]) {
        let mut bytes = Vec::new();
        for rec in records {
            serde_json::to_writer(&mut bytes, rec).unwrap();
            bytes.push(b'\n');
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn build_restart_intents(dir: &std::path::Path) -> (PathBuf, Vec<IntentRecord>) {
        let intent_path = dir.join("intent.jsonl");
        let mut log = IntentLog::open(&intent_path).unwrap();

        // Turn 1: engine brief containing the handoff write credential.
        let i1 = log
            .append_queued_at(
                1_795_720_000_315,
                PromptOrigin::Engine,
                format!("Session brief with auth {TEST_SECRET}"),
                None,
                Vec::new(),
            )
            .unwrap();
        let s1 = log
            .append_sent_at(1_795_720_000_318, i1.intent_id(), RequestId::Number(3))
            .unwrap();

        // Turn 2: operator prompt.
        let i2 = log
            .append_queued_at(
                1_795_720_000_990,
                PromptOrigin::Operator,
                "Check coordinator status",
                Some("op-msg-2".to_owned()),
                Vec::new(),
            )
            .unwrap();
        let s2 = log
            .append_sent_at(1_795_720_000_995, i2.intent_id(), RequestId::Number(4))
            .unwrap();

        // Turn 3: operator prompt that is still streaming at seq 21 when the engine restarts.
        let i3 = log
            .append_queued_at(
                1_795_720_002_990,
                PromptOrigin::Operator,
                "Handle the third request",
                Some("op-msg-3".to_owned()),
                Vec::new(),
            )
            .unwrap();
        let s3 = log
            .append_sent_at(1_795_720_002_995, i3.intent_id(), RequestId::Number(5))
            .unwrap();

        // While Turn 3 is in flight: an operator prompt is queued and withdrawn,
        // another operator prompt with an image stays queued, and an engine
        // prompt is queued (which must take priority over the queued operator prompt).
        let i4_withdrawn = log
            .append_queued_at(
                1_795_720_003_110,
                PromptOrigin::Operator,
                "Never mind this one",
                Some("op-msg-withdrawn".to_owned()),
                Vec::new(),
            )
            .unwrap();
        let w4 = log
            .append_withdrawn_at(
                1_795_720_003_120,
                i4_withdrawn.intent_id(),
                WithdrawReason::CallerWithdrawn,
            )
            .unwrap();

        let i5_queued_op = log
            .append_queued_at(
                1_795_720_003_130,
                PromptOrigin::Operator,
                "Queued operator message with screenshot",
                Some("op-msg-queued".to_owned()),
                vec![PromptImageRef {
                    path: "images/shot-1.png".to_owned(),
                    mime_type: "image/png".to_owned(),
                    filename: Some("shot-1.png".to_owned()),
                    byte_len: Some(2048),
                }],
            )
            .unwrap();

        let i6_queued_eng = log
            .append_queued_at(
                1_795_720_003_140,
                PromptOrigin::Engine,
                "Prompt-changed nudge",
                None,
                Vec::new(),
            )
            .unwrap();

        (
            intent_path,
            vec![i1, s1, i2, s2, i3, s3, i4_withdrawn, w4, i5_queued_op, i6_queued_eng],
        )
    }

    #[test]
    fn queued_message_and_prompt_origins_survive_simulated_engine_restart() {
        let dir = tempfile::tempdir().unwrap();
        let wire_records = read_records(RESTART_FIXTURE.as_bytes()).expect("restart fixture must parse");
        assert_eq!(wire_records.len(), 24);
        let (intent_path, intents) = build_restart_intents(dir.path());
        let redactor = CredentialRedactor::new(TEST_SECRET);

        // Process 1 runs live up through seq 21 (mid-Turn-3 streaming), taking
        // a snapshot at seq 14 (after Turn 2, intent seq 4) before crashing at seq 21.
        let mut live = ConversationReducer::new(redactor.clone());
        // Apply intents 0..4 and wire 0..14 (through Turn 2).
        for intent in &intents[..4] {
            live.apply_intent_record(intent);
        }
        for rec in &wire_records[..14] {
            live.apply_record(rec);
        }
        let snapshot_path = dir.path().join("snapshot.json");
        save_snapshot(&snapshot_path, &live.snapshot()).unwrap();
        let snap_mode = std::fs::metadata(&snapshot_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(snap_mode, 0o600, "snapshot must be private 0600");

        // Continue live through agent-started turn (seq 15..18) and mid-Turn-3 (seq 19..21).
        for rec in &wire_records[14..18] {
            live.apply_record(rec);
            if rec.seq == 15 || rec.seq == 16 {
                assert!(
                    !live.queue().open_agent_turns().is_empty(),
                    "seq {} must keep agent turn open in queue",
                    rec.seq
                );
            }
        }
        assert!(live.queue().open_agent_turns().is_empty());
        for intent in &intents[4..] {
            live.apply_intent_record(intent);
        }
        for rec in &wire_records[18..21] {
            live.apply_record(rec);
        }

        // Write wire journal up to crash point (seq 21) and simulate Process 2 restarting.
        let wire_path = dir.path().join("wire.jsonl");
        write_wire_file(&wire_path, &wire_records[..21]);

        let recovered = recover_from_files(
            &wire_path,
            Some(&intent_path),
            Some(&snapshot_path),
            redactor.clone(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .expect("recovery must succeed");
        assert_eq!(
            recovered.snapshot_status,
            SnapshotLoadStatus::Hit {
                wire_seq: 14,
                intent_seq: 4
            }
        );
        assert_eq!(recovered.wire_records_replayed, 7);
        assert_eq!(recovered.intent_records_replayed, 6);
        assert!(recovered.recovered_unsent_intents.is_empty());

        // Snapshot + tail replay must match live AND full replay from scratch.
        let full = replay_with_intent_log(&wire_records[..21], &intents, redactor.clone(), DEFAULT_TOOL_TEXT_LIMIT);
        for candidate in [&recovered.reducer, &full] {
            assert_eq!(candidate.state(), live.state());
            assert_eq!(candidate.items(), live.items());
            assert_eq!(candidate.in_progress_message(), live.in_progress_message());
            assert_eq!(candidate.queue(), live.queue());
            assert_eq!(candidate.pending_client_request_ids(), vec![RequestId::Number(5)]);
        }

        // Verify Turn 1 engine brief was redacted and pinned turn origin to Engine
        // despite Claude Code's usage_update claiming `human`.
        let items = recovered.reducer.items();
        let items_json = serde_json::to_string(items).unwrap();
        assert!(!items_json.contains(TEST_SECRET), "secret leaked: {items_json}");

        let prompt_1 = recovered.reducer.item_by_id("prompt-5").expect("Turn 1 prompt item");
        match &prompt_1.kind {
            CoordinatorChatItemKind::Message(msg) => {
                assert_eq!(msg.role, CoordinatorChatMessageRole::Engine);
                assert_eq!(msg.text, format!("Session brief with auth {REDACTED_PLACEHOLDER}"));
            }
            other => panic!("expected Engine message, got {other:?}"),
        }
        let turn_1 = recovered.reducer.item_by_id("turn-8").expect("Turn 1 boundary");
        match &turn_1.kind {
            CoordinatorChatItemKind::TurnBoundary(b) => {
                assert_eq!(b.origin, Some(CoordinatorChatTurnOrigin::Engine));
            }
            other => panic!("expected TurnBoundary, got {other:?}"),
        }

        // Verify Turn 2 operator prompt preserved client_message_id and Human origin.
        let prompt_2 = recovered.reducer.item_by_id("prompt-9").expect("Turn 2 prompt item");
        match &prompt_2.kind {
            CoordinatorChatItemKind::Message(msg) => {
                assert_eq!(msg.role, CoordinatorChatMessageRole::Operator);
                assert_eq!(msg.client_message_id.as_deref(), Some("op-msg-2"));
            }
            other => panic!("expected Operator message, got {other:?}"),
        }

        // Verify Turn 3 is in flight, partial stream survived, and queued prompts
        // survived in priority order (Engine ahead of Operator, withdrawn absent).
        let queue = recovered.reducer.queue();
        assert_eq!(queue.in_flight().unwrap().request_id, RequestId::Number(5));
        assert_eq!(queue.in_flight().unwrap().wire_seq, Some(19));
        assert_eq!(
            recovered.reducer.in_progress_message().unwrap().text,
            "Working on your third request "
        );
        assert_eq!(queue.queued_len(), 2);
        assert_eq!(queue.queued()[0].origin, PromptOrigin::Engine);
        assert_eq!(queue.queued()[0].text(), "Prompt-changed nudge");
        assert_eq!(queue.queued()[1].origin, PromptOrigin::Operator);
        assert_eq!(queue.queued()[1].client_message_id.as_deref(), Some("op-msg-queued"));
        assert_eq!(queue.queued()[1].images().len(), 1);
        assert_eq!(queue.queued()[1].images()[0].path, "images/shot-1.png");

        // Now feed the remaining live wire records (seq 22..24) after restart.
        let mut resumed = recovered.reducer;
        for rec in &wire_records[21..] {
            resumed.apply_record(rec);
        }
        assert_eq!(resumed.state().phase, CoordinatorChatConnectionPhase::Idle);
        assert!(resumed.in_progress_message().is_none());
        assert!(resumed.queue().can_send());
        assert_eq!(
            resumed.queue().peek_next_dispatchable().unwrap().origin,
            PromptOrigin::Engine
        );

        let completed_turn_3 = resumed.item_by_id("msg-20").expect("Turn 3 finalized message");
        match &completed_turn_3.kind {
            CoordinatorChatItemKind::Message(msg) => {
                assert_eq!(msg.text, "Working on your third request after restart.");
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_preserves_split_credential_redaction_holdback_across_restart() {
        let redactor = CredentialRedactor::new(TEST_SECRET);
        let (first_half, second_half) = TEST_SECRET.split_at(TEST_SECRET.len() / 2);

        let wire_records = [
            wire(
                1,
                Direction::ToAgent,
                json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
            ),
            wire(
                2,
                Direction::FromAgent,
                json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "s",
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "messageId": "m1",
                            "content": {"type": "text", "text": format!("auth={first_half}")}
                        }
                    }
                }),
            ),
            wire(
                3,
                Direction::FromAgent,
                json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "s",
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "messageId": "m1",
                            "content": {"type": "text", "text": format!("{second_half} ok")}
                        }
                    }
                }),
            ),
            wire(
                4,
                Direction::FromAgent,
                json!({"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}),
            ),
        ];

        // Take snapshot right after seq 2 while `first_half` is held back.
        let mut pre_restart = ConversationReducer::new(redactor.clone());
        pre_restart.apply_record(&wire_records[0]);
        pre_restart.apply_record(&wire_records[1]);
        assert_eq!(pre_restart.in_progress_message().unwrap().text, "auth=");

        let snap_bytes = serde_json::to_vec(&pre_restart.snapshot()).unwrap();
        // The secret list itself is never stored in the snapshot; only the
        // held-back chunk fragment is present in the 0600 cache.
        assert!(!String::from_utf8_lossy(&snap_bytes).contains(TEST_SECRET));

        let recovered = recover_from_slices(
            &wire_records,
            &[],
            Some(&snap_bytes),
            redactor,
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        );
        assert_eq!(
            recovered.snapshot_status,
            SnapshotLoadStatus::Hit {
                wire_seq: 2,
                intent_seq: 0
            }
        );
        let msg = recovered.reducer.item_by_id("msg-2").expect("finalized message");
        match &msg.kind {
            CoordinatorChatItemKind::Message(m) => {
                assert_eq!(m.text, format!("auth={REDACTED_PLACEHOLDER} ok"));
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn missing_corrupt_version_mismatched_and_stale_snapshots_fall_back_to_full_replay() {
        let dir = tempfile::tempdir().unwrap();
        let wire_records = read_records(RESTART_FIXTURE.as_bytes()).unwrap();
        let wire_path = dir.path().join("wire.jsonl");
        write_wire_file(&wire_path, &wire_records);
        let (intent_path, intents) = build_restart_intents(dir.path());
        let redactor = CredentialRedactor::new(TEST_SECRET);
        let baseline = replay_with_intent_log(&wire_records, &intents, redactor.clone(), DEFAULT_TOOL_TEXT_LIMIT);

        // 1. Missing snapshot.
        let missing_path = dir.path().join("does-not-exist.json");
        assert_eq!(load_snapshot(&missing_path), SnapshotLoad::Missing);
        let res = recover_from_files(
            &wire_path,
            Some(&intent_path),
            Some(&missing_path),
            redactor.clone(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert_eq!(res.snapshot_status, SnapshotLoadStatus::Missing);
        assert_eq!(res.wire_records_replayed, wire_records.len());
        assert_eq!(res.reducer.items(), baseline.items());
        assert_eq!(res.reducer.queue(), baseline.queue());

        // 2. Corrupt JSON snapshot (truncated / garbage).
        let corrupt_path = dir.path().join("corrupt.json");
        std::fs::write(&corrupt_path, b"{\"schema_version\":1,\"wire_seq\":").unwrap();
        assert!(matches!(load_snapshot(&corrupt_path), SnapshotLoad::Corrupt { .. }));
        let res = recover_from_files(
            &wire_path,
            Some(&intent_path),
            Some(&corrupt_path),
            redactor.clone(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert!(matches!(res.snapshot_status, SnapshotLoadStatus::Corrupt { .. }));
        assert_eq!(res.wire_records_replayed, wire_records.len());
        assert_eq!(res.reducer.items(), baseline.items());

        // 3. Structurally corrupt snapshot (valid JSON, out-of-bounds index).
        let mut bad_snap = baseline.snapshot();
        bad_snap.projection.compaction_indices.insert("fake".to_owned(), 9999);
        let bad_index_path = dir.path().join("bad-index.json");
        save_snapshot(&bad_index_path, &bad_snap).unwrap();
        let res = recover_from_files(
            &wire_path,
            Some(&intent_path),
            Some(&bad_index_path),
            redactor.clone(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert!(matches!(res.snapshot_status, SnapshotLoadStatus::Corrupt { .. }));
        assert_eq!(res.wire_records_replayed, wire_records.len());
        assert_eq!(res.reducer.items(), baseline.items());

        // 4. Version-mismatched snapshot with an unfamiliar future body layout.
        let future_bytes = br#"{"schema_version":999,"completely_different_field":true}"#;
        assert_eq!(
            load_snapshot_bytes(future_bytes),
            SnapshotLoad::VersionMismatch {
                found: 999,
                expected: SNAPSHOT_SCHEMA_VERSION,
            }
        );
        let mismatch_path = dir.path().join("future.json");
        std::fs::write(&mismatch_path, future_bytes).unwrap();
        let res = recover_from_files(
            &wire_path,
            Some(&intent_path),
            Some(&mismatch_path),
            redactor.clone(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert_eq!(
            res.snapshot_status,
            SnapshotLoadStatus::VersionMismatch {
                found: 999,
                expected: SNAPSHOT_SCHEMA_VERSION,
            }
        );
        assert_eq!(res.wire_records_replayed, wire_records.len());
        assert_eq!(res.reducer.items(), baseline.items());

        // 5. Stale snapshot whose wire_seq is ahead of the journal.
        let mut ahead = baseline.snapshot();
        ahead.cursor.wire_seq = 9_999;
        ahead.projection.state.last_seq = 9_999;
        let ahead_path = dir.path().join("ahead.json");
        save_snapshot(&ahead_path, &ahead).unwrap();
        let res = recover_from_files(
            &wire_path,
            Some(&intent_path),
            Some(&ahead_path),
            redactor.clone(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert!(matches!(
            res.snapshot_status,
            SnapshotLoadStatus::StaleAheadOfJournal {
                snapshot_wire_seq: 9_999,
                ..
            }
        ));
        assert_eq!(res.wire_records_replayed, wire_records.len());
        assert_eq!(res.reducer.items(), baseline.items());
    }

    #[test]
    fn sent_intent_missing_from_wire_is_requeued_and_reused_request_ids_match_fifo() {
        let intents = vec![
            IntentRecord {
                seq: 1,
                ts_ms: 1,
                kind: IntentRecordKind::Queued {
                    intent_id: 1,
                    origin: PromptOrigin::Engine,
                    text: "first turn engine".to_owned(),
                    client_message_id: None,
                    images: Vec::new(),
                },
            },
            IntentRecord {
                seq: 2,
                ts_ms: 2,
                kind: IntentRecordKind::Sent {
                    intent_id: 1,
                    request_id: RequestId::Number(4),
                },
            },
            // Suppose a fresh connection after restart reused numeric id 4.
            IntentRecord {
                seq: 3,
                ts_ms: 3,
                kind: IntentRecordKind::Queued {
                    intent_id: 2,
                    origin: PromptOrigin::Operator,
                    text: "second turn operator".to_owned(),
                    client_message_id: Some("cm-2".to_owned()),
                    images: Vec::new(),
                },
            },
            IntentRecord {
                seq: 4,
                ts_ms: 4,
                kind: IntentRecordKind::Sent {
                    intent_id: 2,
                    request_id: RequestId::Number(4),
                },
            },
            // Third prompt was marked Sent right before crash and never hit the wire.
            IntentRecord {
                seq: 5,
                ts_ms: 5,
                kind: IntentRecordKind::Queued {
                    intent_id: 3,
                    origin: PromptOrigin::Operator,
                    text: "crashed before host journalled".to_owned(),
                    client_message_id: Some("cm-3".to_owned()),
                    images: Vec::new(),
                },
            },
            IntentRecord {
                seq: 6,
                ts_ms: 6,
                kind: IntentRecordKind::Sent {
                    intent_id: 3,
                    request_id: RequestId::Number(5),
                },
            },
        ];

        let wire_records = vec![
            wire(
                1,
                Direction::ToAgent,
                json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
            ),
            wire(
                2,
                Direction::FromAgent,
                json!({"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}),
            ),
            wire(
                3,
                Direction::ToAgent,
                json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
            ),
            wire(
                4,
                Direction::FromAgent,
                json!({"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}),
            ),
        ];

        let recovered = recover_from_slices(
            &wire_records,
            &intents,
            None,
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        );
        assert_eq!(recovered.recovered_unsent_intents.len(), 1);
        assert_eq!(recovered.recovered_unsent_intents[0].intent_id, 3);
        assert_eq!(recovered.reducer.queue().queued_len(), 1);
        assert_eq!(recovered.reducer.queue().queued()[0].intent_id, 3);

        // First prompt-1 is Engine, second prompt-3 is Operator.
        match &recovered.reducer.item_by_id("prompt-1").unwrap().kind {
            CoordinatorChatItemKind::Message(m) => {
                assert_eq!(m.role, CoordinatorChatMessageRole::Engine);
                assert_eq!(m.text, "first turn engine");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match &recovered.reducer.item_by_id("prompt-3").unwrap().kind {
            CoordinatorChatItemKind::Message(m) => {
                assert_eq!(m.role, CoordinatorChatMessageRole::Operator);
                assert_eq!(m.text, "second turn operator");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Generates a synthetic wire journal + intent log sized from the spike's
    /// real per-turn profile (mono#3922):
    /// - 27,587-byte `available_commands_update` lines (the spike's measured
    ///   maximum line size)
    /// - ~25–30 wire records per turn: prompt, tool call + fenced output,
    ///   18–24 streaming `agent_message_chunk`s, `usage_update`, prompt
    ///   response, plus periodic agent-started background turns.
    fn build_synthetic_spike_journal(turns: usize) -> (Vec<WireRecord>, Vec<IntentRecord>) {
        let mut wire_records = Vec::with_capacity(turns * 28 + 10);
        let mut intents = Vec::with_capacity(turns * 2);
        let mut seq: u64 = 0;
        let mut intent_seq: u64 = 0;
        let mut intent_id: u64 = 0;
        let mut req_id: i64 = 0;

        let mut push_wire = |dir: Direction, line: String| {
            seq += 1;
            wire_records.push(WireRecord {
                seq,
                ts_ms: 1_795_720_000_000 + seq * 50,
                dir,
                line,
                invalid: None,
            });
        };

        // Session handshake + a real-sized 27,587-byte `available_commands_update`.
        req_id += 1;
        push_wire(
            Direction::ToAgent,
            json!({"jsonrpc":"2.0","id":req_id,"method":"initialize","params":{"protocolVersion":1}}).to_string(),
        );
        push_wire(
            Direction::FromAgent,
            json!({
                "jsonrpc":"2.0",
                "id":req_id,
                "result":{
                    "protocolVersion":1,
                    "agentInfo":{"name":"@agentclientprotocol/claude-agent-acp","version":"0.110.0"},
                    "_meta":{"jetbrains":{"air":{"version":1,"capabilities":["asyncTasks","backgroundSubagents"]}}}
                }
            })
            .to_string(),
        );
        req_id += 1;
        push_wire(
            Direction::ToAgent,
            json!({"jsonrpc":"2.0","id":req_id,"method":"session/new","params":{"cwd":"/tmp/session","mcpServers":[]}})
                .to_string(),
        );
        push_wire(
            Direction::FromAgent,
            json!({
                "jsonrpc":"2.0",
                "id":req_id,
                "result":{"sessionId":"synthetic-bench","modes":{"currentModeId":"auto"},"configOptions":[{"id":"model","currentValue":"opus"}]}
            })
            .to_string(),
        );

        // Build an `available_commands_update` whose JSON line is >= 27,587 bytes.
        let mut commands = Vec::new();
        for idx in 0..71 {
            commands.push(json!({
                "name": format!("command_{idx:02}"),
                "description": format!("Synthetic command {idx:02}: {}", "d".repeat(320)),
                "input": {"hint": "<optional arguments>"}
            }));
        }
        let big_commands_line = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "synthetic-bench",
                "update": {
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": commands
                }
            }
        })
        .to_string();
        assert!(
            big_commands_line.len() >= 27_587,
            "expected >= 27,587-byte commands line, got {}",
            big_commands_line.len()
        );
        push_wire(Direction::FromAgent, big_commands_line.clone());

        for turn in 0..turns {
            if turn > 0 && turn % 50 == 0 {
                push_wire(Direction::FromAgent, big_commands_line.clone());
            }
            intent_id += 1;
            req_id += 1;
            let origin = if turn == 0 || turn % 25 == 0 {
                PromptOrigin::Engine
            } else {
                PromptOrigin::Operator
            };
            let turn_ts_ms = 1_795_720_000_000 + turn as u64 * 2_000;
            intent_seq += 1;
            intents.push(IntentRecord {
                seq: intent_seq,
                ts_ms: turn_ts_ms,
                kind: IntentRecordKind::Queued {
                    intent_id,
                    origin,
                    text: format!("Turn {turn} prompt text describing coordinator work item status."),
                    client_message_id: (origin == PromptOrigin::Operator).then(|| format!("cm-{turn}")),
                    images: Vec::new(),
                },
            });
            intent_seq += 1;
            intents.push(IntentRecord {
                seq: intent_seq,
                ts_ms: turn_ts_ms + 1,
                kind: IntentRecordKind::Sent {
                    intent_id,
                    request_id: RequestId::Number(req_id),
                },
            });

            push_wire(
                Direction::ToAgent,
                json!({
                    "jsonrpc":"2.0",
                    "id":req_id,
                    "method":"session/prompt",
                    "params":{"sessionId":"synthetic-bench","prompt":[{"type":"text","text":"prompt"}]}
                })
                .to_string(),
            );

            // One tool call + completion with realistic fenced markdown output.
            let tool_id = format!("toolu_synth_{turn}");
            push_wire(
                Direction::FromAgent,
                json!({
                    "jsonrpc":"2.0",
                    "method":"session/update",
                    "params":{
                        "sessionId":"synthetic-bench",
                        "update":{
                            "sessionUpdate":"tool_call",
                            "toolCallId":tool_id,
                            "name":"Bash",
                            "title":format!("boss task list --turn {turn}"),
                            "kind":"execute",
                            "status":"in_progress",
                            "rawInput":{"command":format!("boss task list --turn {turn}")}
                        }
                    }
                })
                .to_string(),
            );
            push_wire(
                Direction::FromAgent,
                json!({
                    "jsonrpc":"2.0",
                    "method":"session/update",
                    "params":{
                        "sessionId":"synthetic-bench",
                        "update":{
                            "sessionUpdate":"tool_call_update",
                            "toolCallId":format!("toolu_synth_{turn}"),
                            "status":"completed",
                            "content":[{"type":"content","content":{"type":"text","text":format!("```json\n{{\"turn\":{turn},\"items\":[\"alpha\",\"beta\",\"gamma\"],\"pad\":\"{}\"}}\n```", "p".repeat(300))}}]
                        }
                    }
                })
                .to_string(),
            );

            // 20 streaming chunks (~300-400 chars per answer, similar to spike median delta size).
            let msg_id = format!("msg_synth_{turn}");
            for chunk_idx in 0..20 {
                push_wire(
                    Direction::FromAgent,
                    json!({
                        "jsonrpc":"2.0",
                        "method":"session/update",
                        "params":{
                            "sessionId":"synthetic-bench",
                            "update":{
                                "sessionUpdate":"agent_message_chunk",
                                "messageId":msg_id,
                                "content":{"type":"text","text":format!("Chunk {chunk_idx} for turn {turn}. ")}
                            }
                        }
                    })
                    .to_string(),
                );
            }

            push_wire(
                Direction::FromAgent,
                json!({
                    "jsonrpc":"2.0",
                    "method":"session/update",
                    "params":{
                        "sessionId":"synthetic-bench",
                        "update":{
                            "sessionUpdate":"usage_update",
                            "used":20_000 + turn as u64 * 150,
                            "size":1_000_000,
                            "cost":{"amount":0.015,"currency":"USD"},
                            "_meta":{"_claude/origin":{"kind":"human"},"_claude/model":"claude-opus-5-5"}
                        }
                    }
                })
                .to_string(),
            );
            push_wire(
                Direction::FromAgent,
                json!({"jsonrpc":"2.0","id":req_id,"result":{"stopReason":"end_turn"}}).to_string(),
            );
        }

        (wire_records, intents)
    }

    #[test]
    fn recovered_unsent_dispatch_retried_with_new_request_id_survives_second_restart() {
        let dir = tempfile::tempdir().unwrap();
        let intent_path = dir.path().join("intent.jsonl");
        let wire_path = dir.path().join("wire.jsonl");
        let snapshot_path = dir.path().join("snapshot.json");

        // Process 1: queues prompt A and logs Sent(A, req 10), then crashes
        // before req 10 reaches the wire journal.
        {
            let mut log = IntentLog::open(&intent_path).unwrap();
            let q = log
                .append_queued_at(
                    100,
                    PromptOrigin::Operator,
                    "retry me once",
                    Some("cm-a".to_owned()),
                    Vec::new(),
                )
                .unwrap();
            log.append_sent_at(101, q.intent_id(), RequestId::Number(10)).unwrap();
        }
        write_wire_file(&wire_path, &[]);

        // Restart 1: recovers A back to `queued` and saves a snapshot.
        let restart1 = recover_from_files(
            &wire_path,
            Some(&intent_path),
            None,
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert_eq!(restart1.recovered_unsent_intents.len(), 1);
        assert_eq!(restart1.reducer.queue().queued_len(), 1);
        save_snapshot(&snapshot_path, &restart1.reducer.snapshot()).unwrap();

        // Process 2: retries A with a new request id (req 11), and it completes on the wire.
        {
            let mut log = IntentLog::open(&intent_path).unwrap();
            log.append_sent_at(200, 1, RequestId::Number(11)).unwrap();
        }
        let wire_records = vec![
            wire(
                1,
                Direction::ToAgent,
                json!({"jsonrpc":"2.0","id":11,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
            ),
            wire(
                2,
                Direction::FromAgent,
                json!({"jsonrpc":"2.0","id":11,"result":{"stopReason":"end_turn"}}),
            ),
        ];
        write_wire_file(&wire_path, &wire_records);

        // Restart 2: both full replay (no snapshot) and snapshot+tail replay must
        // end with an empty queue and the prompt item present exactly once.
        let full = recover_from_files(
            &wire_path,
            Some(&intent_path),
            None,
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        let from_snap = recover_from_files(
            &wire_path,
            Some(&intent_path),
            Some(&snapshot_path),
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert!(matches!(from_snap.snapshot_status, SnapshotLoadStatus::Hit { .. }));

        for result in [&full, &from_snap] {
            assert!(result.recovered_unsent_intents.is_empty());
            assert_eq!(result.reducer.queue().queued_len(), 0);
            assert!(result.reducer.queue().dispatched_awaiting_wire().is_empty());
            assert!(result.reducer.queue().in_flight().is_none());

            let prompt_items: Vec<_> = result
                .reducer
                .items()
                .iter()
                .filter(|item| item.id.starts_with("prompt-"))
                .collect();
            assert_eq!(prompt_items.len(), 1, "prompt item must appear exactly once");
            match &prompt_items[0].kind {
                CoordinatorChatItemKind::Message(msg) => {
                    assert_eq!(msg.role, CoordinatorChatMessageRole::Operator);
                    assert_eq!(msg.text, "retry me once");
                    assert_eq!(msg.client_message_id.as_deref(), Some("cm-a"));
                }
                other => panic!("expected Message, got {other:?}"),
            }
        }
        assert_eq!(full.reducer.state(), from_snap.reducer.state());
        assert_eq!(full.reducer.items(), from_snap.reducer.items());
        assert_eq!(full.reducer.queue(), from_snap.reducer.queue());
    }

    #[test]
    fn recovered_unsent_dispatch_advances_request_id_high_water_mark_across_two_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let intent_path = dir.path().join("intent.jsonl");
        let wire_path = dir.path().join("wire.jsonl");
        let snapshot_path = dir.path().join("snapshot.json");

        // Process 1: wire journal reached request id 9; operator prompt A is
        // queued and logged as Sent(A, 10), then the engine crashes before id 10
        // reaches the wire journal.
        let mut wire_records = vec![
            wire(
                1,
                Direction::ToAgent,
                json!({"jsonrpc":"2.0","id":9,"method":"initialize","params":{"protocolVersion":1}}),
            ),
            wire(
                2,
                Direction::FromAgent,
                json!({"jsonrpc":"2.0","id":9,"result":{"protocolVersion":1}}),
            ),
        ];
        write_wire_file(&wire_path, &wire_records);
        {
            let mut log = IntentLog::open(&intent_path).unwrap();
            let a = log
                .append_queued_at(
                    100,
                    PromptOrigin::Operator,
                    "operator message A",
                    Some("cm-a".to_owned()),
                    Vec::new(),
                )
                .unwrap();
            log.append_sent_at(101, a.intent_id(), RequestId::Number(10)).unwrap();
        }

        // Restart 1: recovers A back to `queued`. The request-id high-water mark
        // must include the abandoned Sent(A, 10) so the next dispatch gets 11.
        let restart1 = recover_from_files(
            &wire_path,
            Some(&intent_path),
            None,
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert_eq!(restart1.recovered_unsent_intents.len(), 1);
        assert_eq!(restart1.recovered_unsent_intents[0].intent_id, 1);
        assert_eq!(restart1.reducer.highest_numeric_client_request_id(), Some(10));
        save_snapshot(&snapshot_path, &restart1.reducer.snapshot()).unwrap();

        // Process 2: higher-priority engine prompt E is queued ahead of A,
        // allocated a fresh request id from the high-water mark (11), reaches
        // the wire, and completes. The engine restarts again before A is retried.
        let next_req_id = restart1.reducer.highest_numeric_client_request_id().unwrap_or(0) + 1;
        assert_eq!(next_req_id, 11);
        {
            let mut log = IntentLog::open(&intent_path).unwrap();
            let e = log
                .append_queued_at(200, PromptOrigin::Engine, "engine prompt E", None, Vec::new())
                .unwrap();
            assert_eq!(e.intent_id(), 2);
            log.append_sent_at(201, e.intent_id(), RequestId::Number(next_req_id))
                .unwrap();
        }
        wire_records.push(wire(
            3,
            Direction::ToAgent,
            json!({"jsonrpc":"2.0","id":next_req_id,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
        ));
        wire_records.push(wire(
            4,
            Direction::FromAgent,
            json!({"jsonrpc":"2.0","id":next_req_id,"result":{"stopReason":"end_turn"}}),
        ));
        write_wire_file(&wire_path, &wire_records);

        // Restart 2: both full replay and snapshot+tail replay must bind the
        // wire prompt to E (Engine origin), leave E completed, keep A queued
        // exactly once, and agree identically.
        let full = recover_from_files(
            &wire_path,
            Some(&intent_path),
            None,
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        let from_snap = recover_from_files(
            &wire_path,
            Some(&intent_path),
            Some(&snapshot_path),
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert!(matches!(from_snap.snapshot_status, SnapshotLoadStatus::Hit { .. }));
        assert_eq!(full.recovered_unsent_intents.len(), 1);
        assert_eq!(full.recovered_unsent_intents[0].intent_id, 1);

        for result in [&full, &from_snap] {
            assert_eq!(result.reducer.highest_numeric_client_request_id(), Some(11));
            assert!(result.reducer.queue().dispatched_awaiting_wire().is_empty());
            assert!(result.reducer.queue().in_flight().is_none());
            assert_eq!(result.reducer.queue().queued_len(), 1);
            assert_eq!(result.reducer.queue().queued()[0].intent_id, 1);
            assert_eq!(result.reducer.queue().queued()[0].origin, PromptOrigin::Operator);
            assert_eq!(result.reducer.queue().queued()[0].text(), "operator message A");

            let prompt_items: Vec<_> = result
                .reducer
                .items()
                .iter()
                .filter(|item| item.id.starts_with("prompt-"))
                .collect();
            assert_eq!(prompt_items.len(), 1, "only E's wire prompt should be emitted");
            match &prompt_items[0].kind {
                CoordinatorChatItemKind::Message(msg) => {
                    assert_eq!(msg.role, CoordinatorChatMessageRole::Engine);
                    assert_eq!(msg.text, "engine prompt E");
                    assert_eq!(msg.client_message_id, None);
                }
                other => panic!("expected Message, got {other:?}"),
            }
            let turn_boundaries: Vec<_> = result
                .reducer
                .items()
                .iter()
                .filter_map(|item| match &item.kind {
                    CoordinatorChatItemKind::TurnBoundary(b) => Some(b),
                    _ => None,
                })
                .collect();
            assert_eq!(turn_boundaries.len(), 1);
            assert_eq!(turn_boundaries[0].origin, Some(CoordinatorChatTurnOrigin::Engine));
        }

        assert_eq!(full.reducer.state(), from_snap.reducer.state());
        assert_eq!(full.reducer.items(), from_snap.reducer.items());
        assert_eq!(full.reducer.queue(), from_snap.reducer.queue());
        assert_eq!(
            full.reducer.highest_numeric_client_request_id(),
            from_snap.reducer.highest_numeric_client_request_id()
        );
    }

    #[test]
    fn durable_requeued_intent_prevents_abandoned_id_collision_across_two_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let intent_path = dir.path().join("intent.jsonl");
        let wire_path = dir.path().join("wire.jsonl");
        let snapshot_path = dir.path().join("snapshot.json");

        // Process 1: operator prompt A is logged as Sent(A, 10), then crashes
        // before id 10 reaches the wire journal.
        write_wire_file(&wire_path, &[]);
        {
            let mut log = IntentLog::open(&intent_path).unwrap();
            let a = log
                .append_queued_at(
                    100,
                    PromptOrigin::Operator,
                    "operator message A",
                    Some("cm-a".to_owned()),
                    Vec::new(),
                )
                .unwrap();
            log.append_sent_at(101, a.intent_id(), RequestId::Number(10)).unwrap();
        }

        // Restart 1: recovers A, records durable Requeued(A) in the intent log,
        // applies it to the reducer, and saves a snapshot.
        let mut restart1 = recover_from_files(
            &wire_path,
            Some(&intent_path),
            None,
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert_eq!(restart1.recovered_unsent_intents.len(), 1);
        {
            let mut log = IntentLog::open(&intent_path).unwrap();
            for prompt in &restart1.recovered_unsent_intents {
                let rec = log.append_requeued_at(150, prompt.intent_id).unwrap();
                restart1.reducer.apply_intent_record(&rec);
            }
        }
        save_snapshot(&snapshot_path, &restart1.reducer.snapshot()).unwrap();

        // Process 2: higher-priority engine prompt E reuses A's abandoned id 10
        // and completes on the wire before A is retried.
        {
            let mut log = IntentLog::open(&intent_path).unwrap();
            let e = log
                .append_queued_at(200, PromptOrigin::Engine, "engine prompt E", None, Vec::new())
                .unwrap();
            assert_eq!(e.intent_id(), 2);
            log.append_sent_at(201, e.intent_id(), RequestId::Number(10)).unwrap();
        }
        let wire_records = vec![
            wire(
                1,
                Direction::ToAgent,
                json!({"jsonrpc":"2.0","id":10,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
            ),
            wire(
                2,
                Direction::FromAgent,
                json!({"jsonrpc":"2.0","id":10,"result":{"stopReason":"end_turn"}}),
            ),
        ];
        write_wire_file(&wire_path, &wire_records);

        // Restart 2: even with reused request id 10, durable Requeued(A)
        // removes A's abandoned attempt from the wire join so E binds to id 10,
        // E is not requeued, A remains queued once, and full replay matches
        // snapshot+tail replay identically.
        let full = recover_from_files(
            &wire_path,
            Some(&intent_path),
            None,
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        let from_snap = recover_from_files(
            &wire_path,
            Some(&intent_path),
            Some(&snapshot_path),
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert!(matches!(from_snap.snapshot_status, SnapshotLoadStatus::Hit { .. }));

        for result in [&full, &from_snap] {
            assert!(result.recovered_unsent_intents.is_empty());
            assert!(result.reducer.queue().dispatched_awaiting_wire().is_empty());
            assert!(result.reducer.queue().in_flight().is_none());
            assert_eq!(result.reducer.queue().queued_len(), 1);
            assert_eq!(result.reducer.queue().queued()[0].intent_id, 1);
            assert_eq!(result.reducer.queue().queued()[0].origin, PromptOrigin::Operator);
            assert_eq!(result.reducer.queue().queued()[0].text(), "operator message A");

            let prompt_items: Vec<_> = result
                .reducer
                .items()
                .iter()
                .filter(|item| item.id.starts_with("prompt-"))
                .collect();
            assert_eq!(prompt_items.len(), 1);
            match &prompt_items[0].kind {
                CoordinatorChatItemKind::Message(msg) => {
                    assert_eq!(msg.role, CoordinatorChatMessageRole::Engine);
                    assert_eq!(msg.text, "engine prompt E");
                    assert_eq!(msg.client_message_id, None);
                }
                other => panic!("expected Message, got {other:?}"),
            }
            let turn_boundaries: Vec<_> = result
                .reducer
                .items()
                .iter()
                .filter_map(|item| match &item.kind {
                    CoordinatorChatItemKind::TurnBoundary(b) => Some(b),
                    _ => None,
                })
                .collect();
            assert_eq!(turn_boundaries.len(), 1);
            assert_eq!(turn_boundaries[0].origin, Some(CoordinatorChatTurnOrigin::Engine));
        }

        assert_eq!(full.reducer.state(), from_snap.reducer.state());
        assert_eq!(full.reducer.items(), from_snap.reducer.items());
        assert_eq!(full.reducer.queue(), from_snap.reducer.queue());
        assert_eq!(
            full.reducer.highest_numeric_client_request_id(),
            from_snap.reducer.highest_numeric_client_request_id()
        );
    }

    #[test]
    fn recover_from_files_tolerates_intent_log_truncated_mid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let intent_path = dir.path().join("intent.jsonl");
        let wire_path = dir.path().join("wire.jsonl");

        {
            let mut log = IntentLog::open(&intent_path).unwrap();
            log.append_queued_at(
                10,
                PromptOrigin::Operator,
                "survives",
                Some("c1".to_owned()),
                Vec::new(),
            )
            .unwrap();
        }
        {
            let mut raw = std::fs::OpenOptions::new().append(true).open(&intent_path).unwrap();
            raw.write_all(br#"{"seq":2,"ts_ms":11,"type":"queued","intent_id":2,"origin":"operator","text":"emoji "#)
                .unwrap();
            raw.write_all(&[0xF0, 0x9F]).unwrap();
        }
        write_wire_file(&wire_path, &[]);

        let recovered = recover_from_files(
            &wire_path,
            Some(&intent_path),
            None,
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .expect("recovery must tolerate a final intent record torn mid-UTF-8");
        assert_eq!(recovered.reducer.queue().queued_len(), 1);
        assert_eq!(recovered.reducer.queue().queued()[0].text(), "survives");
    }

    #[test]
    fn recover_from_files_can_defer_unsent_dispatch_reconciliation_until_live_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let intent_path = dir.path().join("intent.jsonl");
        let wire_path = dir.path().join("wire.jsonl");

        {
            let mut log = IntentLog::open(&intent_path).unwrap();
            let q = log
                .append_queued_at(
                    10,
                    PromptOrigin::Operator,
                    "in transit",
                    Some("c1".to_owned()),
                    Vec::new(),
                )
                .unwrap();
            log.append_sent_at(11, q.intent_id(), RequestId::Number(7)).unwrap();
        }
        write_wire_file(&wire_path, &[]);

        // Recovering while the host may still be appending keeps the intent in
        // `dispatched_awaiting_wire` so the engine does not send a duplicate.
        let mut recovered = recover_from_files(
            &wire_path,
            Some(&intent_path),
            None,
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::DeferUntilLiveBoundary,
        )
        .unwrap();
        assert!(recovered.recovered_unsent_intents.is_empty());
        assert_eq!(recovered.reducer.queue().queued_len(), 0);
        assert_eq!(recovered.reducer.queue().dispatched_awaiting_wire().len(), 1);
        assert!(recovered.reducer.queue().is_busy());

        // If the live tail delivers the wire prompt, it joins cleanly without re-sending.
        recovered.reducer.apply_record(&wire(
            1,
            Direction::ToAgent,
            json!({"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
        ));
        assert!(recovered.reducer.queue().dispatched_awaiting_wire().is_empty());
        assert_eq!(recovered.reducer.queue().in_flight().unwrap().prompt.intent_id, 1);
        assert!(recovered.reducer.item_by_id("prompt-1").is_some());
    }

    #[test]
    fn measure_synthetic_spike_sized_journal_replay() {
        let dir = tempfile::tempdir().unwrap();
        let turns = 250;
        let (wire_records, intents) = build_synthetic_spike_journal(turns);
        let wire_path = dir.path().join("wire.jsonl");
        write_wire_file(&wire_path, &wire_records);
        let intent_path = dir.path().join("intent.jsonl");
        {
            let mut bytes = Vec::new();
            for rec in &intents {
                serde_json::to_writer(&mut bytes, rec).unwrap();
                bytes.push(b'\n');
            }
            std::fs::write(&intent_path, bytes).unwrap();
        }

        let wire_bytes = std::fs::metadata(&wire_path).unwrap().len();
        let intent_bytes = std::fs::metadata(&intent_path).unwrap().len();
        let max_line_bytes = wire_records.iter().map(|r| r.line.len()).max().unwrap_or(0);
        assert!(max_line_bytes >= 27_587);

        let redactor = CredentialRedactor::new(TEST_SECRET);

        // Full replay from disk (no snapshot).
        let full_start = Instant::now();
        let full = recover_from_files(
            &wire_path,
            Some(&intent_path),
            None,
            redactor.clone(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        let full_elapsed = full_start.elapsed();

        // Build snapshot covering 96% of the journal (240 turns), leaving a 10-turn tail.
        let tail_wire_records = 10 * 25;
        let snap_wire_cut = wire_records.len() - tail_wire_records;
        let snap_intent_cut = intents.len() - 20;
        let pre_snap = replay_with_intent_log(
            &wire_records[..snap_wire_cut],
            &intents[..snap_intent_cut],
            redactor.clone(),
            DEFAULT_TOOL_TEXT_LIMIT,
        );
        let snapshot_path = dir.path().join("snapshot.json");
        let save_start = Instant::now();
        save_snapshot(&snapshot_path, &pre_snap.snapshot()).unwrap();
        let save_elapsed = save_start.elapsed();
        let snap_bytes = std::fs::metadata(&snapshot_path).unwrap().len();

        // Snapshot + 10-turn tail replay from disk (reading full files).
        let tail_start = Instant::now();
        let tailed = recover_from_files(
            &wire_path,
            Some(&intent_path),
            Some(&snapshot_path),
            redactor.clone(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        let tail_elapsed = tail_start.elapsed();

        // Snapshot load + host-seeked 10-turn tail (the live agent-host reconnect path).
        let snap_disk_bytes = std::fs::read(&snapshot_path).unwrap();
        let host_tail_start = Instant::now();
        let host_tailed = recover_from_snapshot_and_tail(
            &snap_disk_bytes,
            &wire_records[snap_wire_cut..],
            &intents[snap_intent_cut..],
            redactor.clone(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        let host_tail_elapsed = host_tail_start.elapsed();

        assert!(matches!(tailed.snapshot_status, SnapshotLoadStatus::Hit { .. }));
        assert_eq!(tailed.wire_records_replayed, tail_wire_records);
        assert_eq!(tailed.intent_records_replayed, 20);
        for candidate in [&tailed.reducer, &host_tailed.reducer] {
            assert_eq!(candidate.state(), full.reducer.state());
            assert_eq!(candidate.items(), full.reducer.items());
            assert_eq!(candidate.queue(), full.reducer.queue());
        }

        eprintln!(
            "REPLAY_BENCH: turns={turns} wire_records={} wire_bytes={wire_bytes} intent_records={} intent_bytes={intent_bytes} max_line_bytes={max_line_bytes} full_replay_ms={:.2} snapshot_bytes={snap_bytes} snapshot_save_ms={:.2} file_tail_replay_ms={:.2} host_tail_replay_ms={:.2} tail_wire_records={tail_wire_records}",
            wire_records.len(),
            intents.len(),
            full_elapsed.as_secs_f64() * 1000.0,
            save_elapsed.as_secs_f64() * 1000.0,
            tail_elapsed.as_secs_f64() * 1000.0,
            host_tail_elapsed.as_secs_f64() * 1000.0,
        );
    }

    fn prompt_wire(seq: u64, id: i64) -> WireRecord {
        wire(
            seq,
            Direction::ToAgent,
            json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{"sessionId":"s","prompt":[]}}),
        )
    }

    fn prompt_done_wire(seq: u64, id: i64, stop_reason: &str) -> WireRecord {
        wire(
            seq,
            Direction::FromAgent,
            json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":stop_reason}}),
        )
    }

    fn cancel_wire(seq: u64) -> WireRecord {
        wire(
            seq,
            Direction::ToAgent,
            json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}),
        )
    }

    fn intent_record(seq: u64, kind: IntentRecordKind) -> IntentRecord {
        IntentRecord {
            seq,
            ts_ms: 1_795_720_000_000 + seq,
            kind,
        }
    }

    fn queued(seq: u64, intent_id: u64, origin: PromptOrigin, text: &str) -> IntentRecord {
        intent_record(
            seq,
            IntentRecordKind::Queued {
                intent_id,
                origin,
                text: text.to_owned(),
                client_message_id: None,
                images: Vec::new(),
            },
        )
    }

    #[test]
    fn withdrawn_record_never_clears_a_sent_prompt_that_may_be_on_the_wire() {
        let intents = [
            queued(1, 1, PromptOrigin::Engine, "stale reset prompt"),
            intent_record(
                2,
                IntentRecordKind::Sent {
                    intent_id: 1,
                    request_id: RequestId::Number(1),
                },
            ),
            intent_record(
                3,
                IntentRecordKind::Withdrawn {
                    intent_id: 1,
                    reason: WithdrawReason::StaleOnRestart,
                },
            ),
            queued(4, 2, PromptOrigin::Operator, "next"),
        ];
        let echo = prompt_wire(1, 1);

        let mut live = ConversationReducer::new(CredentialRedactor::none());
        for intent in &intents {
            live.apply_intent_record(intent);
        }
        // The adapter may already be running the withdrawn prompt: the queue
        // must stay busy and must not offer the next prompt for dispatch.
        assert!(live.queue().is_busy());
        assert!(live.queue().peek_next_dispatchable().is_none());
        assert_eq!(live.queue().dispatched_awaiting_wire().len(), 1);

        // Its wire echo still binds, so it gets an item and a turn origin.
        live.apply_record(&echo);
        assert_eq!(live.queue().in_flight().unwrap().prompt.intent_id, 1);
        assert!(live.item_by_id("prompt-1").is_some());
        assert!(live.queue().peek_next_dispatchable().is_none());

        // Replay (all intents before any wire record) agrees with live.
        let replayed = replay_with_intent_log(&[echo], &intents, CredentialRedactor::none(), DEFAULT_TOOL_TEXT_LIMIT);
        assert_eq!(replayed.items(), live.items());
        assert_eq!(replayed.queue(), live.queue());
    }

    #[test]
    fn requeued_then_withdrawn_prompt_leaves_the_queue_idle() {
        let mut reducer = ConversationReducer::new(CredentialRedactor::none());
        for intent in [
            queued(1, 1, PromptOrigin::Engine, "stale"),
            intent_record(
                2,
                IntentRecordKind::Sent {
                    intent_id: 1,
                    request_id: RequestId::Number(1),
                },
            ),
            intent_record(3, IntentRecordKind::Requeued { intent_id: 1 }),
            intent_record(
                4,
                IntentRecordKind::Withdrawn {
                    intent_id: 1,
                    reason: WithdrawReason::StaleOnRestart,
                },
            ),
        ] {
            reducer.apply_intent_record(&intent);
        }
        assert!(!reducer.queue().is_busy());
        assert_eq!(reducer.queue().queued_len(), 0);
    }

    #[test]
    fn wire_prompt_bound_to_no_intent_keeps_the_queue_busy_until_it_finishes() {
        let mut reducer = ConversationReducer::new(CredentialRedactor::none());
        reducer.apply_intent_record(&queued(1, 1, PromptOrigin::Operator, "waiting"));
        assert!(reducer.queue().can_send());

        reducer.apply_record(&prompt_wire(1, 9));
        assert!(reducer.state().phase == CoordinatorChatConnectionPhase::Working);
        assert!(reducer.queue().is_busy());
        assert!(!reducer.queue().can_send(), "must not dispatch mid-turn");

        // A snapshot taken mid-turn keeps the queue busy after a restart.
        let snapshot = reducer.snapshot();
        let restored = ConversationReducer::from_snapshot(snapshot, CredentialRedactor::none()).unwrap();
        assert_eq!(restored.queue(), reducer.queue());
        assert!(!restored.queue().can_send());

        reducer.apply_record(&prompt_done_wire(2, 9, "end_turn"));
        assert!(!reducer.queue().is_busy());
        assert!(reducer.queue().can_send());
    }

    #[test]
    fn stop_withdrawals_survive_restart_and_engine_prompts_dispatch_after_cancelled_turn() {
        let dir = tempfile::tempdir().unwrap();
        let intent_path = dir.path().join("intent.jsonl");
        let wire_path = dir.path().join("wire.jsonl");
        let mut log = IntentLog::open(&intent_path).unwrap();
        let mut reducer = ConversationReducer::new(CredentialRedactor::none());
        let apply = |reducer: &mut ConversationReducer, record: IntentRecord| {
            reducer.apply_intent_record(&record);
        };

        // Operator prompt 1 is running; operator 2 and engine 3 queue behind it.
        let q1 = log
            .append_queued_at(10, PromptOrigin::Operator, "running", None, Vec::new())
            .unwrap();
        apply(&mut reducer, q1.clone());
        let s1 = log.append_sent_at(11, q1.intent_id(), RequestId::Number(1)).unwrap();
        apply(&mut reducer, s1);
        let mut wire_records = vec![prompt_wire(1, 1)];
        reducer.apply_record(&wire_records[0]);
        let q2 = log
            .append_queued_at(12, PromptOrigin::Operator, "draft to return", None, Vec::new())
            .unwrap();
        apply(&mut reducer, q2.clone());
        let q3 = log
            .append_queued_at(13, PromptOrigin::Engine, "engine nudge", None, Vec::new())
            .unwrap();
        apply(&mut reducer, q3.clone());

        // Production Stop path: plan, journal a withdrawal per returned
        // operator prompt, apply it, then the cancel goes out on the wire.
        let plan = reducer.queue().plan_stop();
        assert!(plan.active_turn && plan.send_cancel);
        assert_eq!(plan.returned_operator_prompts.len(), 1);
        assert_eq!(plan.returned_operator_prompts[0].intent_id, q2.intent_id());
        for prompt in &plan.returned_operator_prompts {
            let withdrawn = log
                .append_withdrawn_at(14, prompt.intent_id, WithdrawReason::StopReturnedToComposer)
                .unwrap();
            apply(&mut reducer, withdrawn);
        }
        wire_records.push(cancel_wire(2));
        reducer.apply_record(&wire_records[1]);
        assert!(reducer.queue().cancel_requested());
        assert_eq!(reducer.queue().queued().len(), 1);
        assert_eq!(reducer.queue().queued()[0].intent_id, q3.intent_id());
        assert!(reducer.queue().peek_next_dispatchable().is_none());

        // Restart mid-cancel: the returned operator prompt stays withdrawn.
        write_wire_file(&wire_path, &wire_records);
        let mid = recover_from_files(
            &wire_path,
            Some(&intent_path),
            None,
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert_eq!(mid.reducer.queue(), reducer.queue());

        // The cancelled turn's response arrives: only the engine prompt remains
        // and it is next to dispatch.
        wire_records.push(prompt_done_wire(3, 1, "cancelled"));
        reducer.apply_record(&wire_records[2]);
        assert_eq!(
            reducer.queue().peek_next_dispatchable().unwrap().intent_id,
            q3.intent_id()
        );

        write_wire_file(&wire_path, &wire_records);
        let recovered = recover_from_files(
            &wire_path,
            Some(&intent_path),
            None,
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap();
        assert_eq!(recovered.reducer.queue(), reducer.queue());
        assert_eq!(recovered.reducer.items(), reducer.items());
        let next = recovered.reducer.queue().peek_next_dispatchable().unwrap();
        assert_eq!(next.origin, PromptOrigin::Engine);
        assert_eq!(next.intent_id, q3.intent_id());
    }

    #[test]
    fn recover_from_files_tolerates_wire_journal_truncated_mid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let wire_path = dir.path().join("wire.jsonl");
        write_wire_file(&wire_path, &[prompt_wire(1, 1)]);
        {
            let mut raw = std::fs::OpenOptions::new().append(true).open(&wire_path).unwrap();
            raw.write_all(br#"{"seq":2,"ts_ms":11,"dir":"from_agent","line":"emoji "#)
                .unwrap();
            raw.write_all(&[0xF0, 0x9F]).unwrap();
        }
        let recovered = recover_from_files(
            &wire_path,
            None,
            None,
            CredentialRedactor::none(),
            DEFAULT_TOOL_TEXT_LIMIT,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .expect("recovery must tolerate a final wire record torn mid-UTF-8");
        assert_eq!(recovered.wire_records_replayed, 1);
    }

    #[test]
    fn snapshot_with_different_tool_text_limit_is_a_cache_miss_and_misses_use_the_requested_limit() {
        let limit = 60;
        let long_output = "x".repeat(500);
        let wire_records = [
            wire(
                1,
                Direction::FromAgent,
                json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{
                    "sessionUpdate":"tool_call","toolCallId":"t1","title":"run","kind":"execute","status":"in_progress"}}}),
            ),
            wire(
                2,
                Direction::FromAgent,
                json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{
                    "sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed",
                    "content":[{"type":"content","content":{"type":"text","text":long_output}}]}}}),
            ),
        ];

        // Snapshot written under the default limit, recovered under a custom one.
        let mut default_limit = ConversationReducer::new(CredentialRedactor::none());
        default_limit.apply_record(&wire_records[0]);
        let snapshot_bytes = serde_json::to_vec(&default_limit.snapshot()).unwrap();

        let miss = recover_from_slices(
            &wire_records,
            &[],
            Some(&snapshot_bytes),
            CredentialRedactor::none(),
            limit,
            UnsentDispatchPolicy::ReconcileNow,
        );
        assert_eq!(
            miss.snapshot_status,
            SnapshotLoadStatus::ToolTextLimitMismatch {
                found: DEFAULT_TOOL_TEXT_LIMIT,
                expected: limit
            }
        );
        let tail_err = recover_from_snapshot_and_tail(
            &snapshot_bytes,
            &wire_records[1..],
            &[],
            CredentialRedactor::none(),
            limit,
            UnsentDispatchPolicy::ReconcileNow,
        )
        .unwrap_err();
        assert!(matches!(tail_err, SnapshotLoadStatus::ToolTextLimitMismatch { .. }));

        // A cache hit under the same limit projects exactly what the miss does.
        let mut custom = ConversationReducer::with_tool_text_limit(CredentialRedactor::none(), limit);
        custom.apply_record(&wire_records[0]);
        let custom_bytes = serde_json::to_vec(&custom.snapshot()).unwrap();
        let hit = recover_from_slices(
            &wire_records,
            &[],
            Some(&custom_bytes),
            CredentialRedactor::none(),
            limit,
            UnsentDispatchPolicy::ReconcileNow,
        );
        assert!(matches!(hit.snapshot_status, SnapshotLoadStatus::Hit { .. }));
        assert_eq!(hit.reducer.items(), miss.reducer.items());
        let replayed = replay_with_intent_log(&wire_records, &[], CredentialRedactor::none(), limit);
        assert_eq!(replayed.items(), miss.reducer.items());
        let output = miss.reducer.items().iter().find_map(|item| match &item.kind {
            CoordinatorChatItemKind::ToolCall(tool) => tool.output.clone(),
            _ => None,
        });
        assert!(output.unwrap().len() < long_output.len(), "tool output must be bounded");
    }
}
