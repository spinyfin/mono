use boss_protocol::{
    CoordinatorChatAuthState, CoordinatorChatConnectionPhase, CoordinatorChatConnectionState,
    CoordinatorChatInProgressMessage, CoordinatorChatItem, CoordinatorChatItemKind, CoordinatorChatMessageItem,
    CoordinatorChatMessageRole, CoordinatorChatNoticeItem, CoordinatorChatNoticeKind, CoordinatorChatPermissionOption,
    CoordinatorChatPermissionOptionKind, CoordinatorChatPermissionRequestItem, CoordinatorChatPermissionResolution,
    CoordinatorChatThoughtItem, CoordinatorChatToolCallItem, CoordinatorChatToolKind, CoordinatorChatToolStatus,
    CoordinatorChatTurnBoundaryItem, CoordinatorChatUsage,
};

use crate::redact::{CredentialRedactor, REDACTED_PLACEHOLDER, StreamingRedactor};

const TEST_SECRET: &str = "handoff-cred-9f8e7d6c5b4a3210";

#[test]
fn redacts_every_chat_item_kind_and_text_field() {
    let redactor = CredentialRedactor::new(TEST_SECRET);
    let mut items = vec![
        CoordinatorChatItem {
            id: "msg-1".to_owned(),
            seq: 1,
            updated_seq: 1,
            ts_ms: 1,
            kind: CoordinatorChatItemKind::Message(CoordinatorChatMessageItem {
                role: CoordinatorChatMessageRole::Engine,
                text: format!("Run `boss handoff write --authorization {TEST_SECRET} -`"),
                client_message_id: Some("client-1".to_owned()),
                message_id: Some("m-1".to_owned()),
                sender: Some(format!("peer-{TEST_SECRET}")),
            }),
        },
        CoordinatorChatItem {
            id: "thought-2".to_owned(),
            seq: 2,
            updated_seq: 2,
            ts_ms: 2,
            kind: CoordinatorChatItemKind::Thought(CoordinatorChatThoughtItem {
                text: format!("Using credential {TEST_SECRET} now"),
            }),
        },
        CoordinatorChatItem {
            id: "tool-3".to_owned(),
            seq: 3,
            updated_seq: 4,
            ts_ms: 3,
            kind: CoordinatorChatItemKind::ToolCall(
                CoordinatorChatToolCallItem::builder()
                    .tool_call_id("toolu_1")
                    .title(format!("boss handoff write --authorization {TEST_SECRET} -"))
                    .tool_kind(CoordinatorChatToolKind::Execute)
                    .status(CoordinatorChatToolStatus::Completed)
                    .tool_name(format!("Bash-{TEST_SECRET}"))
                    .input(format!(
                        r#"{{"command":"boss handoff write --authorization {TEST_SECRET} -"}}"#
                    ))
                    .output(format!("```text\nwrote with {TEST_SECRET}\n```"))
                    .non_execution_kind(format!("rule-{TEST_SECRET}"))
                    .build(),
            ),
        },
        CoordinatorChatItem {
            id: "perm-5".to_owned(),
            seq: 5,
            updated_seq: 6,
            ts_ms: 5,
            kind: CoordinatorChatItemKind::PermissionRequest(
                CoordinatorChatPermissionRequestItem::builder()
                    .request_id("0")
                    .title(format!("Run with {TEST_SECRET}"))
                    .input(format!("--authorization {TEST_SECRET}"))
                    .options(vec![CoordinatorChatPermissionOption {
                        option_id: "allow".to_owned(),
                        option_kind: CoordinatorChatPermissionOptionKind::AllowOnce,
                        name: format!("Allow {TEST_SECRET}"),
                    }])
                    .resolution(CoordinatorChatPermissionResolution::Selected {
                        option_id: "allow".to_owned(),
                        option_name: Some(format!("Allow {TEST_SECRET}")),
                    })
                    .build(),
            ),
        },
        CoordinatorChatItem {
            id: "turn-7".to_owned(),
            seq: 7,
            updated_seq: 7,
            ts_ms: 7,
            kind: CoordinatorChatItemKind::TurnBoundary(CoordinatorChatTurnBoundaryItem {
                agent_initiated: false,
                origin: None,
                stop_reason: Some(format!("err-{TEST_SECRET}")),
                error: Some(format!("failed with {TEST_SECRET}")),
            }),
        },
        CoordinatorChatItem {
            id: "notice-8".to_owned(),
            seq: 8,
            updated_seq: 8,
            ts_ms: 8,
            kind: CoordinatorChatItemKind::Notice(CoordinatorChatNoticeItem {
                notice_kind: CoordinatorChatNoticeKind::Error,
                text: format!("error carrying {TEST_SECRET}"),
                pre_tokens: None,
                post_tokens: None,
                trigger: Some(format!("trigger-{TEST_SECRET}")),
            }),
        },
        CoordinatorChatItem {
            id: "usage-9".to_owned(),
            seq: 9,
            updated_seq: 9,
            ts_ms: 9,
            kind: CoordinatorChatItemKind::Usage(CoordinatorChatUsage {
                used: 100,
                size: 1000,
                cost_amount: Some(0.01),
                cost_currency: Some(format!("USD-{TEST_SECRET}")),
                model: Some(format!("model-{TEST_SECRET}")),
            }),
        },
        CoordinatorChatItem {
            id: "unknown-10".to_owned(),
            seq: 10,
            updated_seq: 10,
            ts_ms: 10,
            kind: CoordinatorChatItemKind::Unknown,
        },
    ];

    for item in &mut items {
        redactor.redact_item(item);
        let serialized = serde_json::to_string(item).unwrap();
        assert!(
            !serialized.contains(TEST_SECRET),
            "item {} leaked secret: {serialized}",
            item.id
        );
        if !matches!(item.kind, CoordinatorChatItemKind::Unknown) {
            assert!(
                serialized.contains(REDACTED_PLACEHOLDER),
                "item {} should contain placeholder: {serialized}",
                item.id
            );
        }
    }
}

#[test]
fn redacts_connection_state_and_in_progress_snapshot() {
    let redactor = CredentialRedactor::new(TEST_SECRET);
    let mut state = CoordinatorChatConnectionState::builder()
        .auth_state(CoordinatorChatAuthState::Authenticated)
        .phase(CoordinatorChatConnectionPhase::Failed {
            reason: format!("boom {TEST_SECRET}"),
        })
        .agent_name(format!("agent-{TEST_SECRET}"))
        .agent_version(format!("1.0-{TEST_SECRET}"))
        .mode(format!("auto-{TEST_SECRET}"))
        .model(format!("opus-{TEST_SECRET}"))
        .usage(CoordinatorChatUsage {
            used: 10,
            size: 100,
            cost_amount: None,
            cost_currency: Some(format!("USD-{TEST_SECRET}")),
            model: Some(format!("opus-{TEST_SECRET}")),
        })
        .build();
    redactor.redact_connection_state(&mut state);
    let serialized = serde_json::to_string(&state).unwrap();
    assert!(!serialized.contains(TEST_SECRET), "state leaked secret: {serialized}");

    let mut in_progress = CoordinatorChatInProgressMessage {
        item_id: "msg-1".to_owned(),
        started_seq: 1,
        updated_seq: 2,
        role: CoordinatorChatMessageRole::Coordinator,
        text: format!("streaming {TEST_SECRET} tail"),
    };
    redactor.redact_in_progress(&mut in_progress);
    assert_eq!(in_progress.text, format!("streaming {REDACTED_PLACEHOLDER} tail"));
}

#[test]
fn streaming_redactor_holds_back_tail_for_every_two_chunk_split() {
    let redactor = CredentialRedactor::new(TEST_SECRET);
    assert_eq!(redactor.holdback_chars(), TEST_SECRET.chars().count() - 1);

    let full = format!("prefix before {TEST_SECRET} suffix after");
    let expected = redactor.redact_str(&full);
    assert_eq!(expected, format!("prefix before {REDACTED_PLACEHOLDER} suffix after"));

    // Split at every single character boundary across the entire message,
    // including every position inside the secret itself.
    let boundaries: Vec<usize> = full.char_indices().map(|(idx, _)| idx).collect();
    for split in boundaries {
        let (first, second) = full.split_at(split);
        let mut stream = StreamingRedactor::new(redactor.clone());
        let mut reconstructed = String::new();

        let (offset1, delta1) = stream.push(first);
        assert_eq!(offset1, 0);
        reconstructed.push_str(&delta1);
        assert_eq!(stream.safe_text(), reconstructed);
        // No prefix of the secret can have been exposed before the match closed.
        let secret_prefix = &TEST_SECRET[..4];
        assert!(
            !reconstructed.contains(secret_prefix),
            "split={split}: leaked secret prefix in chunk 1: {reconstructed:?}"
        );

        let (offset2, delta2) = stream.push(second);
        assert_eq!(offset2, reconstructed.len(), "offsets must advance monotonically");
        reconstructed.push_str(&delta2);
        assert_eq!(stream.safe_text(), reconstructed);
        assert!(
            !reconstructed.contains(secret_prefix),
            "split={split}: leaked secret prefix after chunk 2: {reconstructed:?}"
        );

        let (flush_offset, flushed, final_text) = stream.finish();
        assert_eq!(flush_offset, reconstructed.len());
        reconstructed.push_str(&flushed);
        assert_eq!(reconstructed, expected);
        assert_eq!(final_text, expected);
    }
}

#[test]
fn streaming_redactor_handles_single_char_chunks_and_multibyte_utf8() {
    let secret = "clé-secrète-🔐-42";
    let redactor = CredentialRedactor::new(secret);
    let full = format!("héllo {secret} — fin!");
    let expected = format!("héllo {REDACTED_PLACEHOLDER} — fin!");

    let mut stream = StreamingRedactor::new(redactor);
    let mut assembled = String::new();
    for ch in full.chars() {
        let mut buf = [0u8; 4];
        let piece = ch.encode_utf8(&mut buf);
        let (offset, delta) = stream.push(piece);
        assert_eq!(offset, assembled.len());
        assembled.push_str(&delta);
        assert_eq!(stream.safe_text(), assembled);
        assert!(!assembled.contains("clé-s"));
    }
    let (flush_offset, flushed, final_text) = stream.finish();
    assert_eq!(flush_offset, assembled.len());
    assembled.push_str(&flushed);
    assert_eq!(assembled, expected);
    assert_eq!(final_text, expected);
}

#[test]
fn streaming_redactor_flushes_near_miss_losslessly() {
    let redactor = CredentialRedactor::new(TEST_SECRET);
    let near_miss = format!("almost {}_not_quite", &TEST_SECRET[..TEST_SECRET.len() - 1]);
    let mut stream = StreamingRedactor::new(redactor);
    let mut assembled = String::new();

    for chunk in near_miss.as_bytes().chunks(5) {
        let piece = std::str::from_utf8(chunk).unwrap();
        let (offset, delta) = stream.push(piece);
        assert_eq!(offset, assembled.len());
        assembled.push_str(&delta);
    }
    let (flush_offset, flushed, final_text) = stream.finish();
    assert_eq!(flush_offset, assembled.len());
    assembled.push_str(&flushed);
    assert_eq!(assembled, near_miss);
    assert_eq!(final_text, near_miss);
}

#[test]
fn empty_and_duplicate_secrets_are_handled_cleanly() {
    let empty = CredentialRedactor::from_secrets(["", ""]);
    assert!(empty.is_empty());
    assert_eq!(empty.holdback_chars(), 0);
    assert_eq!(empty.redact_str("unchanged"), "unchanged");

    let multi = CredentialRedactor::from_secrets(["short", "short-longer", "short"]);
    assert_eq!(
        multi.redact_str("a short-longer b"),
        format!("a {REDACTED_PLACEHOLDER} b")
    );
}

fn stream_all(redactor: &CredentialRedactor, chunks: &[&str]) -> String {
    let mut stream = StreamingRedactor::new(redactor.clone());
    for chunk in chunks {
        stream.push(chunk);
    }
    stream.finish().2
}

#[test]
fn streaming_overlapping_secrets_match_whole_string_redaction_at_every_split() {
    let redactor = CredentialRedactor::from_secrets(["short", "short-longer", "ort-lo"]);
    for input in [
        "x short-longer y",
        "short-longer",
        "short",
        "shortshort-longer",
        "a short-lon",
        "ort-longer short",
    ] {
        let expected = redactor.redact_str(input);
        for cut in (0..=input.len()).filter(|cut| input.is_char_boundary(*cut)) {
            let (a, b) = input.split_at(cut);
            assert_eq!(stream_all(&redactor, &[a, b]), expected, "input {input:?} cut {cut}");
        }
        let singles: Vec<String> = input.chars().map(String::from).collect();
        let refs: Vec<&str> = singles.iter().map(String::as_str).collect();
        assert_eq!(stream_all(&redactor, &refs), expected, "char-wise {input:?}");
    }
}

#[test]
fn streaming_does_not_emit_prefix_of_longer_overlapping_secret() {
    let redactor = CredentialRedactor::from_secrets(["short", "short-longer"]);
    let mut stream = StreamingRedactor::new(redactor);
    stream.push("short");
    assert!(!stream.safe_text().contains("short"));
    stream.push("-longer");
    assert_eq!(stream.finish().2, REDACTED_PLACEHOLDER);
}

#[test]
fn opaque_identifier_fields_are_deliberately_not_redacted() {
    let redactor = CredentialRedactor::new(TEST_SECRET);
    let mut item = CoordinatorChatItem {
        id: "msg-1".to_owned(),
        seq: 1,
        updated_seq: 1,
        ts_ms: 1,
        kind: CoordinatorChatItemKind::Message(CoordinatorChatMessageItem {
            role: CoordinatorChatMessageRole::Engine,
            text: format!("secret {TEST_SECRET}"),
            client_message_id: Some(format!("client-{TEST_SECRET}")),
            message_id: Some(format!("m-{TEST_SECRET}")),
            sender: None,
        }),
    };
    redactor.redact_item(&mut item);
    let CoordinatorChatItemKind::Message(message) = &item.kind else {
        unreachable!()
    };
    assert_eq!(message.text, format!("secret {REDACTED_PLACEHOLDER}"));
    assert_eq!(
        message.client_message_id.as_deref(),
        Some(format!("client-{TEST_SECRET}").as_str())
    );
    assert_eq!(message.message_id.as_deref(), Some(format!("m-{TEST_SECRET}").as_str()));
}
