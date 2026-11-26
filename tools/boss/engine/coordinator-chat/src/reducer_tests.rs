use boss_agent_host::journal::{Direction, WireRecord, read_records};
use boss_protocol::{
    CoordinatorChatAuthState, CoordinatorChatConnectionPhase, CoordinatorChatItemKind, CoordinatorChatMessageRole,
    CoordinatorChatNoticeKind, CoordinatorChatPermissionResolution, CoordinatorChatToolStatus,
    CoordinatorChatTurnOrigin,
};
use serde_json::json;

use crate::redact::{CredentialRedactor, REDACTED_PLACEHOLDER};
use crate::reducer::{ConversationReducer, PromptIntent, replay_reader, replay_records, replay_with_intents};

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
    assert_eq!(records.len(), 31);

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
    assert_eq!(state.session_id.as_deref(), Some("sess-scrubbed-01"));
    assert_eq!(state.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(state.mode.as_deref(), Some("auto"));
    assert_eq!(state.last_seq, 31);

    // Account notifications become a state only, never their content.
    let state_json = serde_json::to_string(state).unwrap();
    let items_json = serde_json::to_string(first.items()).unwrap();
    for forbidden in ["redacted-user@example.invalid", "Example Org", "Example Plan"] {
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
            CoordinatorChatItemKind::ToolCall(tool) if tool.tool_call_id == "toolu_bg_child" => Some(tool),
            _ => None,
        })
        .expect("background child tool call must exist");
    assert_eq!(child_tool.parent_tool_call_id.as_deref(), Some("toolu_bg_spawn"));
    assert_eq!(child_tool.status, CoordinatorChatToolStatus::Completed);
    assert_eq!(child_tool.output.as_deref(), Some("```text\nBACKGROUND-DONE-58\n```"));

    // Verify phase stayed Idle while the background agent's own tool call ran
    // outside any turn, then moved to Working on `async_task_spawned` (`turn`),
    // and returned to Idle on `async_task_state_update` (`completed`).
    let mut step_reducer = ConversationReducer::new(CredentialRedactor::none());
    for rec in &records {
        step_reducer.apply_record(rec);
        match rec.seq {
            16..=21 => assert_eq!(
                step_reducer.state().phase,
                CoordinatorChatConnectionPhase::Idle,
                "seq {} must not open a turn",
                rec.seq
            ),
            22..=24 => assert_eq!(
                step_reducer.state().phase,
                CoordinatorChatConnectionPhase::Working,
                "seq {} is inside agent-started turn",
                rec.seq
            ),
            25 | 26 => assert_eq!(
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
            CoordinatorChatItemKind::Message(msg) if msg.message_id.as_deref() == Some("msg-2") => Some(msg),
            _ => None,
        })
        .expect("agent-started turn message must exist");
    assert_eq!(agent_message.role, CoordinatorChatMessageRole::Coordinator);
    assert_eq!(agent_message.text, "Background agent finished: BACKGROUND-DONE-58.");

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
    assert_eq!(agent_turn_boundary.0.seq, 25);
    assert_eq!(agent_turn_boundary.0.updated_seq, 26);

    // Compaction tool call (`compact`) became a Compaction notice, not a ToolCall.
    assert!(
        !first.items().iter().any(|item| matches!(
            &item.kind,
            CoordinatorChatItemKind::ToolCall(tool) if tool.tool_call_id == "toolu_compact_1"
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
    assert_eq!(compaction.pre_tokens, Some(42602));
    assert_eq!(compaction.post_tokens, Some(7073));
}

#[test]
fn replays_permission_allow_cancel_and_agent_withdrawal() {
    let records = read_records(PERMISSION_CANCEL_FIXTURE.as_bytes()).expect("fixture must parse cleanly");
    let mut reducer = ConversationReducer::new(CredentialRedactor::none());

    for rec in &records {
        reducer.apply_record(rec);
        match rec.seq {
            7 | 15 | 22 => assert_eq!(
                reducer.state().phase,
                CoordinatorChatConnectionPhase::WaitingForPermission,
                "seq {} should wait for permission",
                rec.seq
            ),
            8 | 16 | 23 => assert_eq!(
                reducer.state().phase,
                CoordinatorChatConnectionPhase::Working,
                "seq {} should resume working after permission resolved/withdrawn",
                rec.seq
            ),
            17 => assert!(reducer.state().cancel_requested),
            19 => {
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

    // Cancelled tool call carries `non_execution_kind` and status Failed.
    let cancelled_tool = reducer
        .items()
        .iter()
        .find_map(|item| match &item.kind {
            CoordinatorChatItemKind::ToolCall(tool) if tool.tool_call_id == "toolu_cancel" => Some(tool),
            _ => None,
        })
        .expect("cancelled tool must exist");
    assert_eq!(cancelled_tool.status, CoordinatorChatToolStatus::Failed);
    assert_eq!(cancelled_tool.non_execution_kind.as_deref(), Some("permission-rule"));

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

    assert_eq!(reducer.state().last_seq, 7);
    assert_eq!(reducer.state().phase, CoordinatorChatConnectionPhase::Working);

    let in_progress = reducer
        .in_progress_message()
        .expect("partial streamed answer must be exposed in snapshot");
    assert_eq!(in_progress.item_id, "msg-6");
    assert_eq!(in_progress.started_seq, 6);
    assert_eq!(in_progress.updated_seq, 7);
    assert_eq!(in_progress.text, "Partial answer still streaming...");

    // A corrupt record in the middle is still a hard error.
    let corrupt_middle = format!("not-json\n{TRUNCATED_FIXTURE}");
    assert!(replay_reader(corrupt_middle.as_bytes(), CredentialRedactor::none()).is_err());
}

#[test]
fn redacts_credential_in_tool_input_output_permission_and_intent_prompt() {
    let mut reducer = ConversationReducer::new(CredentialRedactor::new(TEST_SECRET));
    reducer.apply_intent_message(PromptIntent {
        seq: 1,
        ts_ms: 1_795_000_000_001,
        role: CoordinatorChatMessageRole::Engine,
        text: format!("Private handoff write authorization: {TEST_SECRET}"),
        client_message_id: None,
    });

    let records = [
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
        reducer.apply_record(record);
        let snapshot = serde_json::to_string(&(reducer.items(), reducer.in_progress_message())).unwrap();
        assert!(!snapshot.contains(first_half), "intermediate leak: {snapshot}");
    }
    reducer.apply_record(&record(
        5,
        Direction::FromAgent,
        json!({"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}),
    ));
    let messages: Vec<_> = reducer
        .items()
        .iter()
        .filter_map(|item| match &item.kind {
            CoordinatorChatItemKind::Message(message) => Some(message.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(messages, vec![format!("token {REDACTED_PLACEHOLDER} done")]);
}

#[test]
fn replay_with_intents_matches_live_interleaved_reduction() {
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
    let intent = || PromptIntent {
        seq: 1,
        ts_ms: 1_795_000_000_001,
        role: CoordinatorChatMessageRole::Operator,
        text: "hello".to_owned(),
        client_message_id: None,
    };
    let mut live = ConversationReducer::new(CredentialRedactor::none());
    live.apply_intent_message(intent());
    for record in &records {
        live.apply_record(record);
    }
    let replayed = replay_with_intents(&records, [intent()], CredentialRedactor::none());
    assert_eq!(replayed.items(), live.items());
    assert_eq!(replayed.state(), live.state());
    assert!(matches!(
        &replayed.items()[0].kind,
        CoordinatorChatItemKind::Message(m) if m.role == CoordinatorChatMessageRole::Operator
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
