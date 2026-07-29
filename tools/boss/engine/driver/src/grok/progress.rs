//! Grok progress normalisation (design T-10): hook payload dialect -> `WorkerEvent`.
//!
//! Grok's hook payload is camelCase, with a snake_case `hookEventName` value
//! and its own tool-name vocabulary — see
//! `tools/boss/docs/investigations/grok-pretooluse-decision-vocabulary-and-tool-name-map.md`
//! and design doc §"Payload shape — the reuse boundary". This module
//! rewrites the raw payload into the canonical snake_case shape
//! [`boss_protocol::normalize_hook_event`] expects and delegates to it —
//! the same dialect-agnostic entry point Claude uses directly — rather than
//! reimplementing `WorkerEvent` construction for a seventh time.
//!
//! Deliberately **not** shared with any other driver's dialect:
//! - Codex's `codex/progress.rs` parses a structurally different wire
//!   (`thread.*`/`turn.*`/`item.*` stdout envelopes, or rollout
//!   `session_meta`/`event_msg`/`response_item` records) — reusing that
//!   parser here would silently misparse Grok's payload shape.
//! - Claude's raw hook payload is already canonical; Grok's needs the
//!   translation step this module owns before it can reach the same
//!   shared function.
//! - The `PreToolUse` guard canonicalisation adapter (`grok/hooks.rs`,
//!   `GROK_HOOK_ADAPTER_SCRIPT`) rewrites the same dialect for a different
//!   purpose (a synchronous in-hook allow/deny decision, executed as a
//!   Python subprocess) and must not import from this module or vice versa
//!   — they are independent processes with independent trust boundaries.
//!   The tool-name / event-name vocabularies below are intentionally
//!   duplicated rather than shared for that reason.

use serde_json::{Map, Value};

use boss_protocol::{NormalizeError, WorkerEvent, normalize_hook_event};

use crate::ProgressSessionNormalizer;

/// Grok's snake_case `hookEventName` values -> Boss's canonical PascalCase
/// `hook_event_name`. All fourteen events documented in Grok's bundled hooks
/// guide; the decision-vocabulary spike observed six
/// (`session_start`, `user_prompt_submit`, `pre_tool_use`, `post_tool_use`,
/// `stop`, `notification`) on the wire. The other eight
/// (`session_end`, `post_tool_use_failure`, `permission_denied`,
/// `stop_failure`, `subagent_start`, `subagent_stop`, `pre_compact`,
/// `post_compact`) are documented but unobserved — included here so a
/// canonicalised payload reaches [`normalize_hook_event`] under its real
/// PascalCase name rather than an untranslated snake_case string; whether
/// that name is one `normalize_hook_event` recognises is a separate
/// question the shared function already answers (`SessionEnd` is; the
/// remaining seven are not — see module tests).
const EVENT_NAME_MAP: &[(&str, &str)] = &[
    ("session_start", "SessionStart"),
    ("user_prompt_submit", "UserPromptSubmit"),
    ("pre_tool_use", "PreToolUse"),
    ("post_tool_use", "PostToolUse"),
    ("stop", "Stop"),
    ("notification", "Notification"),
    ("session_end", "SessionEnd"),
    ("post_tool_use_failure", "PostToolUseFailure"),
    ("permission_denied", "PermissionDenied"),
    ("stop_failure", "StopFailure"),
    ("subagent_start", "SubagentStart"),
    ("subagent_stop", "SubagentStop"),
    ("pre_compact", "PreCompact"),
    ("post_compact", "PostCompact"),
];

/// Empirically confirmed on the wire (decision-vocabulary investigation,
/// Part 2 — tool-name map). Only these three transfer; every other
/// `toolName` (`read_file`, `grep`, `list_dir`, `web_search`,
/// `spawn_subagent`, `get_command_or_subagent_output`, …) is left as-is —
/// the safe default, since nothing downstream depends on those names being
/// Claude-shaped.
const TOOL_NAME_MAP: &[(&str, &str)] = &[
    ("run_terminal_command", "Bash"),
    ("write", "Write"),
    ("search_replace", "Edit"),
];

/// Every other camelCase -> snake_case key rename this module applies.
/// `hookEventName` and `toolName` are handled separately above because their
/// *values*, not just their keys, need translation.
const RENAME_MAP: &[(&str, &str)] = &[
    ("sessionId", "session_id"),
    ("toolInput", "tool_input"),
    ("toolResult", "tool_response"),
    ("stopHookActive", "stop_hook_active"),
    ("transcriptPath", "transcript_path"),
    ("permissionMode", "permission_mode"),
    // `reason` and `lastAssistantMessage` on `Stop`: structurally identical
    // to Claude's own Stop payload, which already carries both fields
    // (`hook_event_name":"Stop","stop_hook_active":false,
    // "last_assistant_message":"..."` — see
    // `codex-as-a-first-class-agent-driver.md`). `normalize_hook_event`'s
    // `Stop` arm reads only `stop_hook_active`; `reason` needs no rename
    // (already snake_case) and `last_assistant_message` is renamed here for
    // shape consistency even though neither is consumed downstream today —
    // the same "present but unused" state Claude's own Stop payload is in.
    ("lastAssistantMessage", "last_assistant_message"),
    ("promptId", "prompt_id"),
    ("workspaceRoot", "workspace_root"),
    ("notificationType", "notification_type"),
];

fn mapped<'a>(table: &[(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    table.iter().find(|(from, _)| *from == key).map(|(_, to)| *to)
}

/// Rewrite one raw Grok hook payload into the canonical snake_case shape
/// [`normalize_hook_event`] expects.
///
/// `sessionId` is renamed to `session_id` here — a third fallback is
/// deliberately **not** added inside `extract_session_identity`
/// (`tools/boss/protocol/src/worker_event.rs:166-171`); that function's
/// `session_id` / `thread_id` pair stays the single, driver-agnostic source
/// of truth for session identity, and this canonicalisation step is what
/// makes Grok's payload satisfy it.
///
/// Nested values (`tool_input`, `tool_response`, …) are copied verbatim —
/// only top-level keys are canonicalised, matching how Codex's rollout
/// normaliser preserves `payload.output` untouched.
fn canonicalize(raw: &Value) -> Result<Value, NormalizeError> {
    let obj = raw
        .as_object()
        .ok_or_else(|| NormalizeError::Malformed("expected Grok hook JSON object".into()))?;

    let mut out = Map::with_capacity(obj.len());
    for (key, value) in obj {
        match key.as_str() {
            "hookEventName" => {
                let raw_event = value.as_str().unwrap_or_default();
                let canonical = mapped(EVENT_NAME_MAP, raw_event).unwrap_or(raw_event);
                out.insert("hook_event_name".to_owned(), Value::String(canonical.to_owned()));
            }
            "toolName" => {
                let raw_tool = value.as_str().unwrap_or_default();
                let canonical = mapped(TOOL_NAME_MAP, raw_tool).unwrap_or(raw_tool);
                out.insert("tool_name".to_owned(), Value::String(canonical.to_owned()));
            }
            _ => {
                let canonical_key = mapped(RENAME_MAP, key).unwrap_or(key.as_str());
                out.insert(canonical_key.to_owned(), value.clone());
            }
        }
    }
    Ok(Value::Object(out))
}

/// Canonicalise one raw Grok hook payload and decode it via the shared,
/// dialect-agnostic [`normalize_hook_event`].
///
/// An unrecognised (but well-formed) `hook_event_name` — Grok documents
/// fourteen events; only seven map onto a [`WorkerEvent`] variant — comes
/// back as [`NormalizeError::UnknownEvent`] from the shared function. That
/// is logged and returned as-is: **ignored-with-logging, not rejected**.
/// Never silently drop a payload that fails to parse for any other reason
/// (malformed JSON, missing session identity, …) — those errors propagate
/// too, so a genuine defect is visible rather than swallowed.
fn normalize_grok_hook_event(raw: &Value) -> Result<WorkerEvent, NormalizeError> {
    let canonical = canonicalize(raw)?;
    normalize_hook_event(&canonical).inspect_err(|err| {
        if let NormalizeError::UnknownEvent(name) = err {
            tracing::debug!(
                event = name,
                "grok hook: ignoring additive/undocumented-for-us event variant"
            );
        }
    })
}

/// Per-reader normaliser for Grok's hook-callback progress stream.
///
/// Unlike Codex's stdout/rollout sessions, Grok's payload is fully
/// self-describing on every single hook invocation (own `sessionId`, own
/// `hookEventName`) — there is no cross-record correlation to own, so this
/// type carries no mutable state today. It exists as a real
/// [`ProgressSessionNormalizer`] (rather than a bare free function) to
/// mirror the per-driver module shape `codex/progress.rs` established and
/// so a future genuinely stateful need (e.g. Stop-skip-on-interrupt
/// recovery, design T-12) has an obvious home rather than bolting state
/// onto a stateless function's call sites.
#[derive(Default)]
pub(super) struct GrokProgressSession {
    _private: (),
}

impl GrokProgressSession {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn normalize(&mut self, raw: &Value) -> Result<WorkerEvent, NormalizeError> {
        normalize_grok_hook_event(raw)
    }
}

impl ProgressSessionNormalizer for GrokProgressSession {
    fn normalize_progress_event(&mut self, raw: &Value) -> Result<WorkerEvent, NormalizeError> {
        self.normalize(raw)
    }

    fn transcript_path_for_session(&mut self, _raw: &Value) -> Option<String> {
        // Follow-on: `transcriptPath` is stamped on every Grok hook payload
        // (design §"Session, turn, and transcript identity"), but wiring it
        // through here is a separate concern from this event normaliser —
        // see `GrokDriver::transcript_path_for_session`.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> GrokProgressSession {
        GrokProgressSession::new()
    }

    // Payloads below are the `stdin` bodies from the decision-vocabulary
    // investigation's committed fixtures:
    // `tools/boss/docs/investigations/grok-pretooluse-decision-vocabulary-artifacts/hook_payloads/*.sample.json`.

    #[test]
    fn session_start_source_new_degrades_to_other_with_no_protocol_widening() {
        // SessionStart.sample.json — Grok's `source: "new"` has no Claude
        // equivalent. Confirms (design §3) that `parse_session_start_source`
        // maps it to `Other` rather than erroring, with no enum widening.
        let raw = json!({
            "hookEventName": "session_start",
            "sessionId": "0c4c0914-5e64-432c-90fa-dcdad9ff5957",
            "cwd": "/private/tmp/grok-t02-vocab/cwd",
            "workspaceRoot": "/private/tmp/grok-t02-vocab/cwd",
            "timestamp": "2026-07-28T00:54:03.902740+00:00",
            "permissionMode": "bypassPermissions",
            "source": "new",
        });
        assert_eq!(
            session().normalize_progress_event(&raw).unwrap(),
            WorkerEvent::SessionStart {
                session_id: "0c4c0914-5e64-432c-90fa-dcdad9ff5957".into(),
                source: boss_protocol::SessionStartSource::Other,
                model: None,
            }
        );
    }

    #[test]
    fn user_prompt_submit_preserves_session_and_prompt_text() {
        // UserPromptSubmit.sample.json.
        let raw = json!({
            "hookEventName": "user_prompt_submit",
            "sessionId": "0c4c0914-5e64-432c-90fa-dcdad9ff5957",
            "cwd": "/private/tmp/grok-t02-vocab/cwd",
            "promptId": "54c84245-f157-43cf-8439-bdf624d1f965",
            "permissionMode": "bypassPermissions",
            "prompt": "<user_query>\ndo the thing\n</user_query>",
        });
        assert_eq!(
            session().normalize_progress_event(&raw).unwrap(),
            WorkerEvent::UserPromptSubmit {
                session_id: "0c4c0914-5e64-432c-90fa-dcdad9ff5957".into(),
                prompt: "<user_query>\ndo the thing\n</user_query>".into(),
            }
        );
    }

    #[test]
    fn pre_tool_use_tool_name_map_covers_the_three_confirmed_wire_names() {
        // PreToolUse.write / .search_replace / .run_terminal_command.sample.json.
        for (wire_name, canonical, tool_input) in [
            (
                "write",
                "Write",
                json!({"file_path": "/tmp/w.txt", "content": "WRITE_OK\n"}),
            ),
            (
                "search_replace",
                "Edit",
                json!({"file_path": "/tmp/w.txt", "old_string": "WRITE_OK", "new_string": "WRITE_EDITED_OK"}),
            ),
            (
                "run_terminal_command",
                "Bash",
                json!({"command": "echo SHELL_OK > x.txt && which git jj gh || true"}),
            ),
        ] {
            let raw = json!({
                "hookEventName": "pre_tool_use",
                "sessionId": "0c4c0914-5e64-432c-90fa-dcdad9ff5957",
                "toolName": wire_name,
                "toolUseId": "call-1",
                "toolInput": tool_input,
            });
            assert_eq!(
                session().normalize_progress_event(&raw).unwrap(),
                WorkerEvent::PreToolUse {
                    session_id: "0c4c0914-5e64-432c-90fa-dcdad9ff5957".into(),
                    tool_name: canonical.into(),
                    tool_input: tool_input.clone(),
                },
                "wire tool name {wire_name} must map to {canonical}"
            );
        }
    }

    #[test]
    fn pre_tool_use_unmapped_tool_name_passes_through_unchanged() {
        let raw = json!({
            "hookEventName": "pre_tool_use",
            "sessionId": "s-1",
            "toolName": "read_file",
            "toolInput": {"file_path": "/tmp/x"},
        });
        let WorkerEvent::PreToolUse { tool_name, .. } = session().normalize_progress_event(&raw).unwrap() else {
            panic!("expected PreToolUse");
        };
        assert_eq!(tool_name, "read_file");
    }

    #[test]
    fn post_tool_use_write_preserves_nested_search_replace_tool_result_verbatim() {
        // PostToolUse.sample.json (write / SearchReplace shape). Nested
        // toolResult content must survive uncanonicalised — only the
        // top-level toolResult -> tool_response key rename applies.
        let tool_result = json!({
            "type": "SearchReplace",
            "EditsApplied": {
                "old_string": "",
                "new_string": "WRITE_OK\n",
                "absolute_path": "/private/tmp/grok-t02-vocab/cwd/toolmap_write.txt",
            }
        });
        let raw = json!({
            "hookEventName": "post_tool_use",
            "sessionId": "0c4c0914-5e64-432c-90fa-dcdad9ff5957",
            "toolName": "write",
            "toolUseId": "call-0",
            "toolInput": {"file_path": "/private/tmp/grok-t02-vocab/cwd/toolmap_write.txt", "content": "WRITE_OK\n"},
            "toolResult": tool_result,
        });
        let WorkerEvent::PostToolUse {
            tool_name,
            tool_response,
            ..
        } = session().normalize_progress_event(&raw).unwrap()
        else {
            panic!("expected PostToolUse");
        };
        assert_eq!(tool_name, "Write");
        assert_eq!(tool_response, tool_result);
    }

    #[test]
    fn post_tool_use_run_terminal_command_preserves_bash_shaped_tool_result_verbatim() {
        // PostToolUse.run_terminal_command.sample.json — distinct `Bash`
        // toolResult shape (byte-array output, exit_code, output_for_prompt).
        let tool_result = json!({
            "type": "Bash",
            "output": [104, 105, 10],
            "output_for_prompt": "exit: 0\nhi\n",
            "exit_code": 0,
            "truncated": false,
        });
        let raw = json!({
            "hookEventName": "post_tool_use",
            "sessionId": "0c4c0914-5e64-432c-90fa-dcdad9ff5957",
            "toolName": "run_terminal_command",
            "toolInput": {"command": "echo hi"},
            "toolResult": tool_result,
        });
        let WorkerEvent::PostToolUse {
            tool_name,
            tool_response,
            ..
        } = session().normalize_progress_event(&raw).unwrap()
        else {
            panic!("expected PostToolUse");
        };
        assert_eq!(tool_name, "Bash");
        assert_eq!(tool_response, tool_result);
    }

    #[test]
    fn stop_maps_stop_hook_active_and_tolerates_unconsumed_reason_fields() {
        // Stop.sample.json — `reason` and `lastAssistantMessage` are present
        // (like Claude's own Stop payload) but not read by
        // `WorkerEvent::Stop`; confirms that presence is harmless, not that
        // the fields disappear from the canonical shape (see next test).
        let raw = json!({
            "hookEventName": "stop",
            "sessionId": "0c4c0914-5e64-432c-90fa-dcdad9ff5957",
            "promptId": "54c84245-f157-43cf-8439-bdf624d1f965",
            "reason": "end_turn",
            "stopHookActive": false,
            "lastAssistantMessage": "TOOLMAP_DONE",
            "backgroundTasks": [],
            "sessionCrons": [],
        });
        assert_eq!(
            session().normalize_progress_event(&raw).unwrap(),
            WorkerEvent::Stop {
                session_id: "0c4c0914-5e64-432c-90fa-dcdad9ff5957".into(),
                stop_hook_active: false,
                stop_reason: boss_protocol::StopReason::Completed,
            }
        );
    }

    #[test]
    fn stop_hook_active_true_is_preserved() {
        let raw = json!({
            "hookEventName": "stop",
            "sessionId": "s-1",
            "stopHookActive": true,
        });
        let WorkerEvent::Stop { stop_hook_active, .. } = session().normalize_progress_event(&raw).unwrap() else {
            panic!("expected Stop");
        };
        assert!(stop_hook_active);
    }

    #[test]
    fn canonicalize_renames_reason_and_last_assistant_message_keys_on_stop() {
        // Lower-level check on the canonical shape itself (design: "Key
        // transfers, casing differs on the second" — `reason` needs no
        // rename, `lastAssistantMessage` does), independent of whether
        // `normalize_hook_event`'s Stop arm currently reads them.
        let raw = json!({
            "hookEventName": "stop",
            "sessionId": "s-1",
            "reason": "end_turn",
            "stopHookActive": false,
            "lastAssistantMessage": "TOOLMAP_DONE",
        });
        let canonical = canonicalize(&raw).unwrap();
        assert_eq!(canonical["reason"], "end_turn");
        assert_eq!(canonical["last_assistant_message"], "TOOLMAP_DONE");
        assert!(canonical.get("lastAssistantMessage").is_none());
    }

    #[test]
    fn notification_carries_message() {
        // Notification.sample.json.
        let raw = json!({
            "hookEventName": "notification",
            "sessionId": "76744658-0402-42f7-a644-68dd52f6dad4",
            "notificationType": "task_complete",
            "message": "Background task completed: 019fa637-a484-7bb1-82d2-81c9a1e1cc21",
            "level": "info",
        });
        assert_eq!(
            session().normalize_progress_event(&raw).unwrap(),
            WorkerEvent::Notification {
                session_id: "76744658-0402-42f7-a644-68dd52f6dad4".into(),
                message: "Background task completed: 019fa637-a484-7bb1-82d2-81c9a1e1cc21".into(),
            }
        );
    }

    #[test]
    fn session_end_maps_onto_worker_event_despite_being_unobserved_in_the_spike() {
        let raw = json!({
            "hookEventName": "session_end",
            "sessionId": "s-1",
            "reason": "exit",
        });
        assert_eq!(
            session().normalize_progress_event(&raw).unwrap(),
            WorkerEvent::SessionEnd {
                session_id: "s-1".into(),
                reason: "exit".into(),
            }
        );
    }

    #[test]
    fn documented_but_unimplemented_events_are_ignored_with_logging_not_rejected() {
        // Seven of the fourteen documented events have no WorkerEvent
        // variant: they must come back as UnknownEvent (logged upstream),
        // never a panic and never a silently-dropped Ok(_) with no signal.
        for wire_name in [
            "post_tool_use_failure",
            "permission_denied",
            "stop_failure",
            "subagent_start",
            "subagent_stop",
            "pre_compact",
            "post_compact",
        ] {
            let raw = json!({
                "hookEventName": wire_name,
                "sessionId": "s-1",
            });
            assert!(
                matches!(
                    session().normalize_progress_event(&raw),
                    Err(NormalizeError::UnknownEvent(_))
                ),
                "expected UnknownEvent for documented-but-unimplemented {wire_name}"
            );
        }
    }

    #[test]
    fn a_future_undocumented_event_name_is_also_tolerated() {
        let raw = json!({
            "hookEventName": "some_future_hook",
            "sessionId": "s-1",
        });
        assert!(matches!(
            session().normalize_progress_event(&raw),
            Err(NormalizeError::UnknownEvent(name)) if name == "some_future_hook"
        ));
    }

    #[test]
    fn missing_session_identity_is_a_real_error_not_a_silent_drop() {
        let raw = json!({
            "hookEventName": "stop",
            "stopHookActive": false,
        });
        assert!(matches!(
            session().normalize_progress_event(&raw),
            Err(NormalizeError::MissingField("session_id"))
        ));
    }

    #[test]
    fn non_object_payload_is_malformed_not_ignored() {
        let raw = json!("not an object");
        assert!(matches!(
            session().normalize_progress_event(&raw),
            Err(NormalizeError::Malformed(_))
        ));
    }

    #[test]
    fn session_id_key_is_canonicalised_before_extract_session_identity_not_as_a_third_fallback() {
        // sessionId is the only identity key Grok ever sends; this pins that
        // the rename happens in this module's canonicalize(), not by
        // widening `extract_session_identity` with a third alias.
        let raw = json!({
            "hookEventName": "stop",
            "sessionId": "renamed-before-shared-parser",
            "stopHookActive": false,
        });
        let WorkerEvent::Stop { session_id, .. } = session().normalize_progress_event(&raw).unwrap() else {
            panic!("expected Stop");
        };
        assert_eq!(session_id, "renamed-before-shared-parser");
    }
}
