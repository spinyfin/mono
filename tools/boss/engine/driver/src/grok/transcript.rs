//! Grok transcript normalisation (design T-11): the ACP `sessionUpdate`
//! dialect written to
//! `$GROK_HOME/sessions/<pct-encoded-cwd>/<sid>/updates.jsonl` -> the
//! canonical redactable shape `boss_engine_live_status_redact` and the
//! live-status summariser expect (`tool_name` / `tool_input` /
//! `tool_response` at the top level; `content[].type` of `text`,
//! `thinking`, or `tool_use` with `name` + `input`).
//!
//! Deliberately **not** shared with any other driver's transcript dialect:
//! - Claude's transcript JSONL is already canonical
//!   (`ClaudeDriver::normalize_transcript_entry` is the identity).
//! - Codex's rollout dialect (`codex/progress.rs`) parses a structurally
//!   different wire (`session_meta` / `event_msg` / `response_item`
//!   records with a `type`+`payload` envelope). Reusing that parser here
//!   would silently misparse Grok's `method` + `params.update` ACP
//!   envelope.
//!
//! `tool_call` and `tool_call_update` arrive as separate records, joined
//! only by `toolCallId` — unlike Claude's `PostToolUse`, which carries a
//! tool's input and result in one payload. [`GrokTranscriptSession`] owns
//! that join, scoped to one tail, mirroring the shape (bounded
//! `HashMap` + eviction `VecDeque`) `codex/progress.rs`'s
//! `CodexTranscriptSession` uses for the same reason on a different wire.
//!
//! Real captured records (redacted esc-interrupt session,
//! `tools/boss/docs/investigations/ghostty-grok-pane-viability-artifacts/ghosttykit_host/evidence/esc_interrupt/session_telemetry_excerpt.md`):
//!
//! ```jsonl
//! {"method":"session/update","params":{"sessionId":"…","update":{
//!   "sessionUpdate":"tool_call","toolCallId":"call-…-0",
//!   "title":"run_terminal_command","rawInput":{"command":"sleep 45",…}}}}
//! {"method":"session/update","params":{"sessionId":"…","update":{
//!   "sessionUpdate":"tool_call_update","toolCallId":"call-…-0",
//!   "status":"in_progress","rawOutput":{"exit_code":0,…}}}}
//! ```
//!
//! Only the *initial* `tool_call` record's `title` is the raw tool
//! identifier (`"run_terminal_command"`); a later `tool_call_update`'s
//! `title` is a rendered description (`"Execute \`sleep 45\`"`), so the
//! tool name is captured once at `tool_call` and reused from correlation
//! state rather than re-derived from every record.

use std::collections::{HashMap, VecDeque};

use serde_json::{Map, Value, json};

use crate::TranscriptSessionNormalizer;

use super::progress::TOOL_NAME_MAP;

/// Bound on in-flight tool calls tracked by [`GrokTranscriptSession`] — the
/// transcript-rendering correlator the live-status loop
/// (`live_status_loop.rs::run_slot_loop`) constructs **once per worker slot
/// and keeps for the life of the run**, feeding it every tail entry across
/// every turn, exactly like `codex/progress.rs::CodexTranscriptSession`. A
/// resolved call is removed by [`GrokTranscriptSession::take`] as soon as its
/// `tool_call_update` arrives, so occupancy only grows from calls that are
/// abandoned (never observed completing) — a long multi-turn Grok session can
/// in principle abandon calls turn after turn with nothing draining this map
/// between them. Sized to match Codex's own transcript-correlator cap
/// (`codex/progress.rs::MAX_TRACKED_TRANSCRIPT_TOOL_CALLS`) for the same
/// reason: make eviction a true tail event rather than something an ordinary
/// long session runs into. Where it must still exist — no cap is infinite —
/// eviction is loud: logged at `error!` in [`GrokTranscriptSession::remember`].
const MAX_TRACKED_GROK_CALLS: usize = 4096;

/// Grok's own tool nomenclature -> Claude-canonical name, reusing the
/// vocabulary `grok/progress.rs` already established for the hook dialect
/// (`run_terminal_command` -> `Bash`, etc.) rather than inventing a second
/// table for the same three tools. This is Grok's own vocabulary, shared
/// within this driver — not the cross-dialect parser reuse the module doc
/// above forbids.
fn mapped_tool_name(raw_tool: &str) -> String {
    TOOL_NAME_MAP
        .iter()
        .find(|(from, _)| *from == raw_tool)
        .map(|(_, to)| (*to).to_owned())
        .unwrap_or_else(|| raw_tool.to_owned())
}

struct PendingToolCall {
    tool_name: String,
    tool_input: Value,
}

enum ProseRole {
    User,
    Thought,
    Message,
}

/// Parsed ACP `sessionUpdate` dialect. Intentionally not reused for the hook
/// payload dialect (`grok/progress.rs`) — a structurally different wire.
enum AcpEnvelope {
    Prose {
        role: ProseRole,
        text: String,
    },
    ToolCall {
        call_id: String,
        tool_name: String,
        tool_input: Value,
    },
    ToolCallUpdate {
        call_id: String,
        output: Option<Value>,
    },
    TurnCompleted {
        stop_reason: Option<String>,
    },
    HookExecution {
        event_name: Option<String>,
    },
    Unknown {
        kind: String,
    },
}

fn extract_text(update: &Map<String, Value>) -> Option<String> {
    update.get("content")?.get("text")?.as_str().map(str::to_owned)
}

fn parse_acp_envelope(obj: &Map<String, Value>) -> Option<AcpEnvelope> {
    let update = obj.get("params")?.get("update")?.as_object()?;
    let kind = update.get("sessionUpdate")?.as_str()?;
    Some(match kind {
        "user_message_chunk" => AcpEnvelope::Prose {
            role: ProseRole::User,
            text: extract_text(update)?,
        },
        "agent_message_chunk" => AcpEnvelope::Prose {
            role: ProseRole::Message,
            text: extract_text(update)?,
        },
        "agent_thought_chunk" => AcpEnvelope::Prose {
            role: ProseRole::Thought,
            text: extract_text(update)?,
        },
        "tool_call" => AcpEnvelope::ToolCall {
            call_id: update.get("toolCallId")?.as_str()?.to_owned(),
            tool_name: update.get("title")?.as_str()?.to_owned(),
            tool_input: update.get("rawInput").cloned().unwrap_or(Value::Null),
        },
        "tool_call_update" => AcpEnvelope::ToolCallUpdate {
            call_id: update.get("toolCallId")?.as_str()?.to_owned(),
            output: update.get("rawOutput").cloned(),
        },
        "turn_completed" => AcpEnvelope::TurnCompleted {
            stop_reason: update.get("stop_reason").and_then(Value::as_str).map(str::to_owned),
        },
        "hook_execution" => AcpEnvelope::HookExecution {
            event_name: update.get("event_name").and_then(Value::as_str).map(str::to_owned),
        },
        other => AcpEnvelope::Unknown { kind: other.to_owned() },
    })
}

fn user_text(text: &str) -> Value {
    json!({"type": "user", "text": text})
}

fn assistant_text(text: &str) -> Value {
    json!({"type": "assistant", "content": [{"type": "text", "text": text}]})
}

fn assistant_thinking(text: &str) -> Value {
    json!({"type": "assistant", "content": [{"type": "thinking", "thinking": text}]})
}

/// Non-prose ACP lifecycle filler, tagged `system` so marker scans and the
/// live-status summariser never mistake it for worker prose — same reason
/// `codex/progress.rs::lifecycle_system` exists for Codex's own fillers.
fn lifecycle_system(subtype: &str, message: &str) -> Value {
    json!({"type": "system", "subtype": subtype, "message": message})
}

/// Per-tail correlation state for one Grok transcript tail.
///
/// A registry driver may be shared by concurrent readers; this type is not
/// — the engine constructs one per tail (`GrokDriver::transcript_session`),
/// so in-flight `tool_call` state from one worker's slot can never leak
/// into another's.
#[derive(Default)]
pub(super) struct GrokTranscriptSession {
    pending: HashMap<String, PendingToolCall>,
    order: VecDeque<String>,
}

impl GrokTranscriptSession {
    fn remember(&mut self, call_id: String, call: PendingToolCall) {
        if let std::collections::hash_map::Entry::Occupied(mut existing) = self.pending.entry(call_id.clone()) {
            existing.insert(call);
            return;
        }
        while self.pending.len() >= MAX_TRACKED_GROK_CALLS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.pending.remove(&oldest);
            tracing::error!(
                cap = MAX_TRACKED_GROK_CALLS,
                "grok updates.jsonl: evicting oldest in-flight tool_call — session has accumulated more than \
                 MAX_TRACKED_GROK_CALLS abandoned/unresolved calls; the evicted call's eventual tool_call_update \
                 will be dropped as unmatched"
            );
        }
        self.pending.insert(call_id.clone(), call);
        self.order.push_back(call_id);
    }

    fn take(&mut self, call_id: &str) -> Option<PendingToolCall> {
        let call = self.pending.remove(call_id)?;
        if let Some(index) = self.order.iter().position(|known| known == call_id) {
            self.order.remove(index);
        }
        Some(call)
    }

    pub(super) fn normalize(&mut self, raw: Value) -> Value {
        let parsed = match raw.as_object().and_then(parse_acp_envelope) {
            Some(parsed) => parsed,
            None => {
                tracing::debug!("grok updates.jsonl: ignoring malformed/non-ACP transcript record");
                return raw;
            }
        };

        match parsed {
            AcpEnvelope::Prose {
                role: ProseRole::User,
                text,
            } => user_text(&text),
            AcpEnvelope::Prose {
                role: ProseRole::Message,
                text,
            } => assistant_text(&text),
            AcpEnvelope::Prose {
                role: ProseRole::Thought,
                text,
            } => assistant_thinking(&text),
            AcpEnvelope::ToolCall {
                call_id,
                tool_name,
                tool_input,
            } => {
                let pending = PendingToolCall {
                    tool_name: mapped_tool_name(&tool_name),
                    tool_input,
                };
                let normalized = json!({
                    "type": "assistant",
                    "content": [{"type": "tool_use", "name": &pending.tool_name, "input": &pending.tool_input}],
                });
                self.remember(call_id, pending);
                normalized
            }
            AcpEnvelope::ToolCallUpdate { call_id, output } => match output {
                None => json!({"type": "system"}),
                Some(tool_response) => match self.take(&call_id) {
                    Some(call) => json!({
                        "type": "user",
                        "tool_name": call.tool_name,
                        "tool_input": call.tool_input,
                        "tool_response": tool_response,
                    }),
                    None => {
                        tracing::debug!(call_id, "grok updates.jsonl: omitting unmatched tool_call_update body");
                        json!({"type": "system"})
                    }
                },
            },
            AcpEnvelope::TurnCompleted { stop_reason } => lifecycle_system(
                "turn_completed",
                &format!("turn completed: {}", stop_reason.as_deref().unwrap_or("unknown")),
            ),
            AcpEnvelope::HookExecution { event_name } => lifecycle_system(
                "hook_execution",
                &format!("hook ran: {}", event_name.as_deref().unwrap_or("unknown")),
            ),
            AcpEnvelope::Unknown { kind } => {
                tracing::debug!(
                    session_update = kind,
                    "grok updates.jsonl: ignoring additive sessionUpdate variant"
                );
                raw
            }
        }
    }
}

impl TranscriptSessionNormalizer for GrokTranscriptSession {
    fn normalize_transcript_entry(&mut self, raw: Value) -> Value {
        self.normalize(raw)
    }
}

/// Reshape one ACP `updates.jsonl` record for a caller that does not
/// maintain cross-record state (direct callers / fixtures). The live-status
/// tail instead calls `GrokDriver::transcript_session`, whose
/// [`GrokTranscriptSession`] correlates `tool_call` / `tool_call_update`
/// pairs across an entire tail; an isolated call here sees at most the
/// `tool_call` half of that join (a bare `tool_use`, no matching
/// `tool_response`), the same trade-off `codex/progress.rs::normalize_rollout`
/// documents for its own isolated-session form.
pub(super) fn normalize_acp_update(raw: Value) -> Value {
    GrokTranscriptSession::default().normalize(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_chunk_maps_to_canonical_user_text() {
        let raw = json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "user_message_chunk",
                "content": {"type": "text", "text": "do the thing"}
            }}
        });
        assert_eq!(
            normalize_acp_update(raw),
            json!({"type": "user", "text": "do the thing"})
        );
    }

    #[test]
    fn agent_message_chunk_maps_to_canonical_assistant_text() {
        let raw = json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "[blocked] reason=\"need a decision\""}
            }}
        });
        assert_eq!(
            normalize_acp_update(raw),
            json!({"type": "assistant", "content": [{"type": "text", "text": "[blocked] reason=\"need a decision\""}]}),
        );
    }

    #[test]
    fn agent_thought_chunk_maps_to_thinking_block() {
        let raw = json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": {"type": "text", "text": "considering options"}
            }}
        });
        assert_eq!(
            normalize_acp_update(raw),
            json!({"type": "assistant", "content": [{"type": "thinking", "thinking": "considering options"}]}),
        );
    }

    #[test]
    fn tool_call_maps_to_tool_use_and_renames_known_grok_tool() {
        let raw = json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": "run_terminal_command",
                "rawInput": {"command": "sleep 45", "description": "Sleep for 45 seconds", "timeout": 60000}
            }}
        });
        assert_eq!(
            normalize_acp_update(raw),
            json!({
                "type": "assistant",
                "content": [{
                    "type": "tool_use",
                    "name": "Bash",
                    "input": {"command": "sleep 45", "description": "Sleep for 45 seconds", "timeout": 60000},
                }],
            }),
        );
    }

    #[test]
    fn tool_call_leaves_unmapped_grok_tool_name_as_is() {
        let raw = json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": "read_file",
                "rawInput": {"path": "/tmp/x"}
            }}
        });
        assert_eq!(normalize_acp_update(raw)["content"][0]["name"], "read_file");
    }

    #[test]
    fn tool_call_update_without_output_is_an_interim_system_filler() {
        let raw = json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "kind": "execute",
                "content": [{"type": "content", "content": {"type": "text", "text": "Sleep for 45 seconds"}}]
            }}
        });
        assert_eq!(normalize_acp_update(raw), json!({"type": "system"}));
    }

    #[test]
    fn tool_call_update_with_no_matching_tool_call_omits_body() {
        let raw = json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-orphan",
                "rawOutput": {"exit_code": 0}
            }}
        });
        assert_eq!(normalize_acp_update(raw), json!({"type": "system"}));
    }

    #[test]
    fn tool_call_and_tool_call_update_correlate_across_records_in_one_tail() {
        let mut session = GrokTranscriptSession::default();

        let call = session.normalize(json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "call-41033b16-0",
                "title": "run_terminal_command",
                "rawInput": {"command": "sleep 45"}
            }}
        }));
        assert_eq!(call["type"], "assistant");
        assert_eq!(call["content"][0]["name"], "Bash");

        // An interim update (no rawOutput yet) must not consume the pending call.
        let interim = session.normalize(json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-41033b16-0",
                "content": [{"type": "content", "content": {"type": "text", "text": "…"}}]
            }}
        }));
        assert_eq!(interim, json!({"type": "system"}));

        let result = session.normalize(json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-41033b16-0",
                "status": "in_progress",
                "rawOutput": {"exit_code": 0, "output": []}
            }}
        }));
        assert_eq!(result["type"], "user");
        assert_eq!(result["tool_name"], "Bash");
        assert_eq!(result["tool_input"], json!({"command": "sleep 45"}));
        assert_eq!(result["tool_response"], json!({"exit_code": 0, "output": []}));

        // The call was consumed; a second matching update finds nothing pending.
        let stale = session.normalize(json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-41033b16-0",
                "rawOutput": {"exit_code": 1}
            }}
        }));
        assert_eq!(stale, json!({"type": "system"}));
    }

    #[test]
    fn turn_completed_maps_to_system_lifecycle_filler() {
        let raw = json!({
            "method": "_x.ai/session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "turn_completed",
                "prompt_id": "p1",
                "stop_reason": "cancelled"
            }}
        });
        let normalized = normalize_acp_update(raw);
        assert_eq!(normalized["type"], "system");
        assert_eq!(normalized["subtype"], "turn_completed");
        assert_eq!(normalized["message"], "turn completed: cancelled");
    }

    #[test]
    fn hook_execution_maps_to_system_lifecycle_filler() {
        let raw = json!({
            "method": "_x.ai/session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "hook_execution",
                "event_name": "pre_tool_use",
                "tool_name": "run_terminal_command",
                "runs": [{"name": "global/dump-all:pre_tool_use[0].hooks[0]", "status": {"status": "success", "elapsed_ms": 17}}]
            }}
        });
        let normalized = normalize_acp_update(raw);
        assert_eq!(normalized["type"], "system");
        assert_eq!(normalized["subtype"], "hook_execution");
        assert_eq!(normalized["message"], "hook ran: pre_tool_use");
    }

    #[test]
    fn passes_through_non_acp_and_malformed_records_unchanged() {
        let already_canonical =
            json!({"type": "assistant", "content": [{"type": "text", "text": "already canonical"}]});
        assert_eq!(normalize_acp_update(already_canonical.clone()), already_canonical);

        let no_params = json!({"foo": "bar"});
        assert_eq!(normalize_acp_update(no_params.clone()), no_params);

        let unknown_kind = json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {"sessionUpdate": "some_future_variant", "x": 1}}
        });
        assert_eq!(normalize_acp_update(unknown_kind.clone()), unknown_kind);
    }
}
