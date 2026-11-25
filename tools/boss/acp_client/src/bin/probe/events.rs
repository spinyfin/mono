//! The probe's event lines: what a front end needs to render the conversation,
//! reduced from the typed protocol messages.
//!
//! Each event that comes from a wire line carries that line's journal `seq`,
//! so an event can always be traced back to the exact line that produced it.
//! Text the agent produced is bounded here; the journal keeps the whole line.

use std::io::Write;

use boss_acp_client::schema::{RequestId, RequestPermissionRequest, SessionConfigOption, SessionUpdate};
use serde_json::{Map, Value, json};

/// Longest tool input or output put on an event line, in characters.
///
/// A front end shows a tool call as one row with an expandable body, so the
/// event carries enough to read and no more. The journal keeps every byte.
const TOOL_TEXT_LIMIT: usize = 4000;

/// Writes one event line to stdout.
pub fn emit(event: Value) {
    let mut stdout = std::io::stdout().lock();
    // A front end that went away is not an error worth dying for mid-turn.
    let _ = writeln!(stdout, "{event}");
    let _ = stdout.flush();
}

/// The current value of each session option, by option id.
pub fn config_summary<'a>(options: impl IntoIterator<Item = &'a SessionConfigOption>) -> Value {
    let mut current = Map::new();
    for option in options {
        let Ok(Value::Object(option)) = serde_json::to_value(option) else {
            continue;
        };
        if let (Some(Value::String(id)), Some(value)) = (option.get("id"), option.get("currentValue")) {
            current.insert(id.clone(), value.clone());
        }
    }
    Value::Object(current)
}

/// Reduces one typed `session/update` to an event.
///
/// `params` is the notification as it arrived. The typed update decides what
/// kind of event this is; the adapter's `_meta` extensions, which the typed
/// schema carries only as an opaque map, are read from `params`.
pub fn session_update(update: &SessionUpdate, params: &Value, seq: u64) -> Value {
    let raw = params.get("update").cloned().unwrap_or(Value::Null);
    let meta = raw.get("_meta").cloned().unwrap_or(Value::Null);
    match update {
        SessionUpdate::AgentMessageChunk(_) => chunk("agent", &raw, seq),
        SessionUpdate::AgentThoughtChunk(_) => chunk("thought", &raw, seq),
        SessionUpdate::UserMessageChunk(_) => chunk("user", &raw, seq),
        SessionUpdate::ToolCall(_) => tool_call(&raw, false, seq),
        SessionUpdate::ToolCallUpdate(_) => tool_call(&raw, true, seq),
        SessionUpdate::CurrentModeUpdate(mode) => json!({
            "event": "mode",
            "mode": mode.current_mode_id,
            "seq": seq,
        }),
        SessionUpdate::UsageUpdate(usage) => json!({
            "event": "usage",
            "used": usage.used,
            "size": usage.size,
            "model": meta.get("_claude/model"),
            "origin": meta.pointer("/_claude~1origin/kind"),
            "seq": seq,
        }),
        // Everything else is reported by kind only. `non_exhaustive` upstream:
        // a kind added by a later schema release lands here too.
        _ => json!({
            "event": "update",
            "kind": raw.get("sessionUpdate"),
            "typed": true,
            "meta_keys": meta.as_object().map(|meta| meta.keys().cloned().collect::<Vec<_>>()),
            "seq": seq,
        }),
    }
}

/// A `session/update` whose kind the typed schema does not have.
///
/// The adapter's task extension is the case that matters: it reports work
/// that outlives a prompt (a background command, a background agent, and the
/// agent's own follow-up turn once one of those finishes) as a task that is
/// spawned and later changes state. Those become `async_task` events; any
/// other unknown kind is reported by name.
pub fn untyped_session_update(params: &Value, seq: u64) -> Value {
    let raw = params.get("update").cloned().unwrap_or(Value::Null);
    let kind = raw.get("sessionUpdate").and_then(Value::as_str).unwrap_or_default();
    let phase = match kind {
        "async_task_spawned" => "spawned",
        "async_task_state_update" => "state",
        "async_task_progress" => "progress",
        _ => return json!({"event": "update", "kind": raw.get("sessionUpdate"), "typed": false, "seq": seq}),
    };
    json!({
        "event": "async_task",
        "phase": phase,
        "id": raw.get("asyncTaskId"),
        "task_type": raw.get("taskType"),
        "state": raw.get("state"),
        "name": raw.get("name"),
        "tool_call_id": raw.get("toolCallId"),
        "seq": seq,
    })
}

fn chunk(role: &str, raw: &Value, seq: u64) -> Value {
    let content = raw.get("content").cloned().unwrap_or(Value::Null);
    json!({
        "event": "message_chunk",
        "role": role,
        "text": content.get("text").and_then(Value::as_str).unwrap_or_default(),
        "content_type": content.get("type"),
        "message_id": raw.get("messageId"),
        "seq": seq,
    })
}

/// A new tool call, or a change to one. Fields the line did not carry are
/// `null`, so a front end merges an update into the row it already has.
fn tool_call(raw: &Value, is_update: bool, seq: u64) -> Value {
    let meta = raw.get("_meta").cloned().unwrap_or(Value::Null);
    json!({
        "event": "tool_call",
        "id": raw.get("toolCallId"),
        "is_update": is_update,
        "title": raw.get("title"),
        "kind": raw.get("kind"),
        "status": raw.get("status"),
        "input": raw.get("rawInput").filter(|input| !input.is_null()).map(|input| bounded(&compact(input))),
        "output": tool_output(raw),
        "tool_name": meta.pointer("/claudeCode/toolName"),
        "parent_tool_call_id": meta.pointer("/claudeCode/parentToolUseId"),
        "meta_keys": meta.as_object().map(|meta| meta.keys().cloned().collect::<Vec<_>>()),
        "seq": seq,
    })
}

/// The text of a tool call's content blocks, else its raw output.
fn tool_output(raw: &Value) -> Option<String> {
    let mut text = String::new();
    for block in raw.get("content").and_then(Value::as_array).into_iter().flatten() {
        let piece = match block.get("type").and_then(Value::as_str) {
            Some("content") => block
                .pointer("/content/text")
                .and_then(Value::as_str)
                .map(str::to_owned),
            Some("diff") => block
                .get("path")
                .and_then(Value::as_str)
                .map(|path| format!("[diff] {path}")),
            Some("terminal") => Some("[terminal]".to_owned()),
            _ => None,
        };
        if let Some(piece) = piece {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&piece);
        }
    }
    if text.is_empty() {
        text = raw.get("rawOutput").filter(|output| !output.is_null()).map(compact)?;
    }
    Some(bounded(&text))
}

/// A permission request, with the agent's own options in the agent's order.
pub fn permission_request(id: &RequestId, request: &RequestPermissionRequest, params: &Value, seq: u64) -> Value {
    let tool_call = params.get("toolCall").cloned().unwrap_or(Value::Null);
    json!({
        "event": "permission_request",
        "request_id": id.to_string(),
        "tool_call_id": tool_call.get("toolCallId"),
        "title": tool_call.get("title"),
        "kind": tool_call.get("kind"),
        "input": tool_call.get("rawInput").filter(|input| !input.is_null()).map(|input| bounded(&compact(input))),
        "options": request.options.iter().map(|option| json!({
            "id": option.option_id,
            "name": option.name,
            "kind": option.kind,
        })).collect::<Vec<_>>(),
        "seq": seq,
    })
}

/// A notification outside `session/update`. Only its shape is reported: these
/// are the adapter's extensions, and the account ones name the signed-in user.
pub fn extension_notification(method: &str, params: &Value, seq: u64) -> Value {
    let mut event = json!({
        "event": "notification",
        "method": method,
        "param_keys": params.as_object().map(|params| params.keys().cloned().collect::<Vec<_>>()),
        "seq": seq,
    });
    if method == "auth/status_update" {
        event["auth_state"] = params.get("state").cloned().unwrap_or(Value::Null);
        event["auth_kinds"] = params
            .get("accounts")
            .and_then(Value::as_array)
            .map(|accounts| {
                accounts
                    .iter()
                    .map(|account| {
                        json!({
                            "active": account.get("active"),
                            "kind": account.pointer("/_meta/claudeCode/kind"),
                            "plan": account.pointer("/_meta/claudeCode/plan"),
                        })
                    })
                    .collect()
            })
            .unwrap_or(Value::Null);
    }
    event
}

fn compact(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Cuts `text` to the event limit on a character boundary and says so.
fn bounded(text: &str) -> String {
    match text.char_indices().nth(TOOL_TEXT_LIMIT) {
        None => text.to_owned(),
        Some((cut, _)) => format!(
            "{}\n… truncated: {} of {} characters shown",
            &text[..cut],
            TOOL_TEXT_LIMIT,
            text.chars().count()
        ),
    }
}

#[cfg(test)]
mod tests {
    use boss_acp_client::JsonRpcMessage;
    use boss_acp_client::schema::SessionNotification;

    use super::*;

    fn reduce(update: Value) -> Value {
        let params = json!({"sessionId": "s", "update": update});
        let notification = SessionNotification::parse_message("session/update", &params)
            .unwrap_or_else(|error| panic!("the typed schema must read {params}: {error}"));
        session_update(&notification.update, &params, 41)
    }

    #[test]
    fn a_streamed_answer_chunk_keeps_its_text_message_and_line() {
        let event = reduce(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "pong"},
            "messageId": "msg_1",
        }));
        assert_eq!(
            event,
            json!({
                "event": "message_chunk",
                "role": "agent",
                "text": "pong",
                "content_type": "text",
                "message_id": "msg_1",
                "seq": 41,
            })
        );
        let thought =
            reduce(json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hm"}}));
        assert_eq!(thought["role"], "thought");
    }

    #[test]
    fn a_tool_call_and_its_update_share_an_id_and_leave_absent_fields_null() {
        let started = reduce(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "toolu_1",
            "title": "boss product list --json",
            "kind": "execute",
            "status": "pending",
            "rawInput": {"command": "boss product list --json"},
            "_meta": {"claudeCode": {"toolName": "Bash"}},
        }));
        assert_eq!(started["id"], "toolu_1");
        assert_eq!(started["is_update"], false);
        assert_eq!(started["input"], r#"{"command":"boss product list --json"}"#);
        assert_eq!(started["tool_name"], "Bash");
        assert_eq!(started["output"], Value::Null);

        let finished = reduce(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "toolu_1",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "[]"}}],
        }));
        assert_eq!(finished["id"], "toolu_1");
        assert_eq!(finished["is_update"], true);
        assert_eq!(finished["status"], "completed");
        assert_eq!(finished["output"], "[]");
        assert_eq!(
            finished["title"],
            Value::Null,
            "an update that does not rename the call must not blank its title"
        );
    }

    #[test]
    fn long_tool_output_is_cut_on_a_character_boundary_and_marked() {
        let long = "é".repeat(TOOL_TEXT_LIMIT + 10);
        let event = reduce(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "toolu_1",
            "content": [{"type": "content", "content": {"type": "text", "text": long}}],
        }));
        let output = event["output"].as_str().unwrap();
        assert!(output.starts_with(&"é".repeat(TOOL_TEXT_LIMIT)));
        assert!(
            output.ends_with(&format!(
                "truncated: {TOOL_TEXT_LIMIT} of {} characters shown",
                TOOL_TEXT_LIMIT + 10
            )),
            "{}",
            &output[output.len() - 80..]
        );
    }

    #[test]
    fn usage_reports_who_started_the_turn_when_the_adapter_says() {
        let event = reduce(json!({
            "sessionUpdate": "usage_update",
            "used": 25979,
            "size": 1000000,
            "_meta": {"_claude/origin": {"kind": "human"}, "_claude/model": "claude-opus-5-5"},
        }));
        assert_eq!(event["event"], "usage");
        assert_eq!(event["origin"], "human");
        assert_eq!(event["model"], "claude-opus-5-5");
        assert_eq!(event["used"], 25979);
    }

    #[test]
    fn the_task_extension_updates_the_typed_schema_lacks_become_task_events() {
        let spawned = json!({"sessionId": "s", "update": {
            "sessionUpdate": "async_task_spawned",
            "asyncTaskId": "autonomous-turn-1",
            "name": "Claude is working",
            "taskType": "turn",
            "canStop": true,
        }});
        assert!(
            SessionNotification::parse_message("session/update", &spawned).is_err(),
            "the typed schema gained this kind: use it instead of the untyped path"
        );
        assert_eq!(
            untyped_session_update(&spawned, 37),
            json!({
                "event": "async_task",
                "phase": "spawned",
                "id": "autonomous-turn-1",
                "task_type": "turn",
                "state": null,
                "name": "Claude is working",
                "tool_call_id": null,
                "seq": 37,
            })
        );

        let finished = json!({"sessionId": "s", "update": {
            "sessionUpdate": "async_task_state_update",
            "asyncTaskId": "autonomous-turn-1",
            "state": "completed",
        }});
        let event = untyped_session_update(&finished, 44);
        assert_eq!(
            (&event["phase"], &event["id"], &event["state"]),
            (&json!("state"), &json!("autonomous-turn-1"), &json!("completed"))
        );

        let unknown = json!({"sessionId": "s", "update": {"sessionUpdate": "something_new"}});
        assert_eq!(
            untyped_session_update(&unknown, 5),
            json!({"event": "update", "kind": "something_new", "typed": false, "seq": 5})
        );
    }

    #[test]
    fn other_updates_are_reported_by_kind() {
        let event = reduce(json!({"sessionUpdate": "session_info_update", "title": "Pong"}));
        assert_eq!(event["event"], "update");
        assert_eq!(event["kind"], "session_info_update");
        assert_eq!(event["typed"], true);
    }

    #[test]
    fn an_account_notification_reports_its_state_and_never_the_account() {
        let event = extension_notification(
            "auth/status_update",
            &json!({
                "state": "authenticated",
                "accounts": [{
                    "label": "Some Plan",
                    "active": true,
                    "email": "person@example.com",
                    "organization": "person@example.com's Organization",
                    "_meta": {"claudeCode": {"kind": "account", "plan": "pro"}},
                }],
            }),
            7,
        );
        assert_eq!(event["auth_state"], "authenticated");
        assert_eq!(
            event["auth_kinds"],
            json!([{"active": true, "kind": "account", "plan": "pro"}])
        );
        assert!(!event.to_string().contains("example.com"), "{event}");
    }
}
