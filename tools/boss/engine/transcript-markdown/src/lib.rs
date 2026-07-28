//! JSONL → markdown transcript converter for agent session logs.
//!
//! Two JSONL dialects are understood, detected from each transcript's own
//! records rather than from the driver that produced it:
//! - Claude Code session logs (`user` / `assistant` / `tool_result` /
//!   `system` records).
//! - Codex rollout files (`session_meta` / `turn_context` / `world_state` /
//!   `event_msg` / `response_item` records).
//!
//! The public API is:
//! - [`parse_transcript`] — JSONL text → [`Vec<TranscriptEvent>`], silently
//!   returning nothing for content whose schema can't be identified (kept
//!   for callers that scan best-effort, e.g. automation heuristics).
//! - [`parse_transcript_checked`] — same, but returns an error when the
//!   schema can't be identified, for callers (the CLI) that must not
//!   present an unrecognised-format transcript as an empty one.
//! - [`parse_transcript_values`] — already-deserialized entries → the same
//!   (for callers that normalize through a driver first).
//! - [`events_to_segments`] — normalized events → [`Vec<TranscriptSegment>`]
//! - [`segments_to_markdown`] — flat document from segments (CLI / single-blob)
//! - [`render_text`] — plain-text rendering for the CLI transcript command
//!
//! # Two accepted Claude-family entry dialects
//!
//! Within the Claude Code schema path, Claude's own transcript wraps turn
//! content in a `message` envelope
//! (`{"type":"assistant","message":{"content":[…]}}`). Every other driver
//! reshapes its native log into the *canonical* entry shape via
//! `AgentDriver::normalize_transcript_entry`, which puts `content` (or a bare
//! `text`) at the top level (`{"type":"assistant","content":[…]}`). Both are
//! parsed here, so a caller that normalizes through the run's driver first
//! (see `boss_engine::driver_transcript`) gets the same events regardless of
//! which agent produced the file. Accepting only the `message` envelope is
//! what made the marker scans — `[blocked]`, `[effort-escalation]`,
//! `[deferred-scope]`, `NO_CHANGES_NEEDED` — silently Claude-only.

use boss_engine_codex_rollout::{
    canonical_rollout_tool_call, canonical_rollout_tool_output, extract_text_blocks as extract_codex_text_blocks,
};
use serde_json::Value;

// ── Public types ──────────────────────────────────────────────────────────────

/// A normalized event parsed from one or more lines of a Claude Code JSONL
/// transcript file.
#[derive(Debug, Clone)]
pub struct TranscriptEvent {
    pub seq: u64,
    pub kind: TranscriptEventKind,
    pub timestamp: Option<String>,
    pub model: Option<String>,
}

/// Discriminated kind for a transcript event.
#[derive(Debug, Clone)]
pub enum TranscriptEventKind {
    UserText(String),
    AssistantText(String),
    Thinking(String),
    ToolUse { name: String, input: Value },
    ToolResult { output: String, is_error: bool },
    System { subtype: Option<String>, body: String },
}

/// One rendered segment, suitable for lazy display in the UI.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct TranscriptSegment {
    pub seq: u64,
    pub role: SegmentRole,
    /// Short human-readable label (e.g. `"User"`, `"⚙ Bash"`, `"↳ result"`).
    pub label: String,
    pub timestamp: Option<String>,
    pub model: Option<String>,
    /// Rendered markdown body for this segment.
    pub markdown: String,
    #[builder(default = false)]
    pub collapsible: bool,
    #[builder(default = false)]
    pub default_collapsed: bool,
    pub truncated: Option<TruncationInfo>,
}

/// Role/origin of a transcript segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentRole {
    User,
    Assistant,
    Thinking,
    Tool,
    System,
}

/// Metadata set when a tool result was truncated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationInfo {
    pub shown_bytes: usize,
    pub total_bytes: usize,
}

/// Options controlling how events are rendered.
#[derive(Debug, Clone)]
pub struct RenderOpts {
    /// Maximum bytes from a single `tool_result` before the output is
    /// truncated and `truncated` is set on the segment.
    pub max_result_bytes: usize,
    /// When true, `tool_use` and `tool_result` segments are omitted from
    /// the output, leaving only user/assistant/thinking/system turns.
    pub hide_tools: bool,
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            max_result_bytes: 8 * 1024,
            hide_tools: false,
        }
    }
}

// ── schema detection ──────────────────────────────────────────────────────────

/// Which JSONL dialect a transcript's records belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptSchema {
    ClaudeCode,
    CodexRollout,
}

/// Returned by [`parse_transcript_checked`] when a transcript's schema
/// could not be identified from its own records — either the content
/// matches no known dialect, or (implausibly) matches more than one.
#[derive(Debug, Clone)]
pub struct UnrecognizedTranscriptSchema {
    pub message: String,
}

impl std::fmt::Display for UnrecognizedTranscriptSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for UnrecognizedTranscriptSchema {}

/// Inspect a transcript's own records to determine which dialect it uses.
///
/// Returns `Ok(None)` for content with no parseable records at all (a
/// genuinely empty transcript, not an unrecognised one).
fn detect_schema(jsonl_content: &str) -> Result<Option<TranscriptSchema>, UnrecognizedTranscriptSchema> {
    let mut claude_hits = 0u32;
    let mut codex_hits = 0u32;
    let mut non_empty_lines = 0u32;
    let mut json_objects = 0u32;
    let mut typed_records = 0u32;
    let mut unknown_types = 0u32;

    for line in jsonl_content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        non_empty_lines += 1;
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if !value.is_object() {
            continue;
        }
        json_objects += 1;
        let Some(type_str) = value.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        typed_records += 1;
        match type_str {
            "user" | "assistant" | "tool_result" | "system" => claude_hits += 1,
            "session_meta" | "turn_context" | "world_state" | "event_msg" | "response_item" => codex_hits += 1,
            _ => unknown_types += 1,
        }
    }

    match (claude_hits > 0, codex_hits > 0) {
        (true, false) => Ok(Some(TranscriptSchema::ClaudeCode)),
        (false, true) => Ok(Some(TranscriptSchema::CodexRollout)),
        (false, false) => {
            if non_empty_lines == 0 {
                Ok(None)
            } else {
                Err(UnrecognizedTranscriptSchema {
                    message: unrecognized_schema_message(non_empty_lines, json_objects, typed_records, unknown_types),
                })
            }
        }
        (true, true) => Err(UnrecognizedTranscriptSchema {
            message: "transcript mixes Claude Code and Codex rollout record types; cannot determine schema".to_owned(),
        }),
    }
}

fn unrecognized_schema_message(
    non_empty_lines: u32,
    json_objects: u32,
    typed_records: u32,
    unknown_types: u32,
) -> String {
    // `bossctl agents transcript` often feeds only the last N lines. When that
    // window contains parseable JSON but no Claude/Codex record types, say so
    // explicitly rather than implying the whole file is an unknown dialect.
    if typed_records > 0 {
        format!(
            "parsed {typed_records} JSON record(s) with a `type` field in the supplied content \
             ({unknown_types} unrecognised type(s)), but none match a known schema \
             (Claude Code: user/assistant/tool_result/system; Codex rollout: session_meta/\
             turn_context/world_state/event_msg/response_item). If this is a tail of a larger \
             transcript, try a larger --lines window so schema-identifying records are included"
        )
    } else if json_objects > 0 {
        format!(
            "parsed {json_objects} JSON object(s) in the supplied content, but none had a \
             recognised schema `type` field (Claude Code or Codex rollout). If this is a tail \
             of a larger transcript, try a larger --lines window"
        )
    } else {
        format!(
            "transcript content has {non_empty_lines} non-empty line(s) but none were \
             parseable JSON objects matching a known schema (Claude Code or Codex rollout)"
        )
    }
}

// ── JSONL parsing ─────────────────────────────────────────────────────────────

/// Parse raw JSONL transcript text into normalized events.
///
/// The schema (Claude Code or Codex rollout) is detected from the
/// transcript's own records. Malformed lines, unrecognised record types
/// within a detected schema, and incomplete trailing lines are silently
/// skipped. Content whose schema can't be identified at all yields an
/// empty result — use [`parse_transcript_checked`] where an unrecognised
/// schema must be reported rather than read as "empty transcript".
pub fn parse_transcript(jsonl_content: &str) -> Vec<TranscriptEvent> {
    parse_transcript_checked(jsonl_content).unwrap_or_default()
}

/// Like [`parse_transcript`], but returns an error instead of an empty
/// result when the transcript's schema can't be identified.
pub fn parse_transcript_checked(jsonl_content: &str) -> Result<Vec<TranscriptEvent>, UnrecognizedTranscriptSchema> {
    match detect_schema(jsonl_content)? {
        Some(TranscriptSchema::ClaudeCode) | None => Ok(parse_transcript_claude(jsonl_content)),
        Some(TranscriptSchema::CodexRollout) => Ok(parse_transcript_codex(jsonl_content)),
    }
}

fn parse_transcript_claude(jsonl_content: &str) -> Vec<TranscriptEvent> {
    let mut events = Vec::new();
    let mut seq: u64 = 0;
    for line in jsonl_content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let new_events = parse_one_value(&value, &mut seq);
        events.extend(new_events);
    }
    events
}

/// Parse already-deserialized transcript entries into normalized events.
///
/// Same semantics as the Claude-family path of [`parse_transcript`], for
/// callers that must transform each entry before parsing — notably a
/// driver-aware read, which routes every line through
/// `AgentDriver::normalize_transcript_entry` so a non-Claude transcript
/// arrives here in the canonical entry shape. Unrecognised entries are
/// skipped, exactly as they are for a raw text parse.
///
/// Accepts any iterator of owned [`Value`]s so a caller can stream normalized
/// entries without materialising a whole-transcript `Vec<Value>` first (see
/// `boss_engine::driver_transcript::parse_transcript_with_driver`).
pub fn parse_transcript_values(values: impl IntoIterator<Item = Value>) -> Vec<TranscriptEvent> {
    let mut events = Vec::new();
    let mut seq: u64 = 0;
    for value in values {
        events.extend(parse_one_value(&value, &mut seq));
    }
    events
}

// ── Codex rollout parsing ─────────────────────────────────────────────────────
//
// Codex rollout files wrap each record as `{"timestamp", "type", "payload"}`.
// `session_meta` and `world_state` are structural/environment dumps (the
// rollout analogue of Claude's `system.subtype=init`) and carry no
// conversation content, so they're dropped. `turn_context` carries the
// active model, tracked across records to attach to subsequent turns.
//
// `response_item` "message" records with role `user`/`developer` duplicate
// `event_msg.user_message` and additionally carry large injected
// boilerplate (AGENTS.md, permissions/plugins instructions) on the first
// turn, so `event_msg.user_message` is used as the clean source of user
// text instead. Conversely `event_msg.agent_message` duplicates the
// `response_item` role=`assistant` message, so the latter is used instead
// since it preserves per-message granularity.

fn parse_transcript_codex(jsonl_content: &str) -> Vec<TranscriptEvent> {
    let mut events = Vec::new();
    let mut seq: u64 = 0;
    let mut current_model: Option<String> = None;

    for line in jsonl_content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(obj) = value.as_object() else {
            continue;
        };
        let Some(record_type) = obj.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        let timestamp = obj.get("timestamp").and_then(|v| v.as_str()).map(|s| s.to_owned());
        let Some(payload) = obj.get("payload").and_then(|v| v.as_object()) else {
            continue;
        };

        match record_type {
            "turn_context" => {
                if let Some(model) = payload.get("model").and_then(|v| v.as_str()) {
                    current_model = Some(model.to_owned());
                }
            }
            "event_msg" => parse_codex_event_msg(payload, timestamp, &mut seq, &mut events),
            "response_item" => parse_codex_response_item(payload, timestamp, &current_model, &mut seq, &mut events),
            _ => {}
        }
    }
    events
}

fn push_codex_event(
    events: &mut Vec<TranscriptEvent>,
    seq: &mut u64,
    kind: TranscriptEventKind,
    timestamp: Option<String>,
    model: Option<String>,
) {
    let s = *seq;
    *seq += 1;
    events.push(TranscriptEvent {
        seq: s,
        kind,
        timestamp,
        model,
    });
}

fn parse_codex_event_msg(
    payload: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    seq: &mut u64,
    events: &mut Vec<TranscriptEvent>,
) {
    let Some(event_type) = payload.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    match event_type {
        "user_message" => {
            if let Some(text) = payload.get("message").and_then(|v| v.as_str()) {
                push_codex_event(
                    events,
                    seq,
                    TranscriptEventKind::UserText(text.to_owned()),
                    timestamp,
                    None,
                );
            }
        }
        "turn_aborted" => {
            let reason = payload.get("reason").and_then(|v| v.as_str()).unwrap_or("interrupted");
            push_codex_event(
                events,
                seq,
                TranscriptEventKind::System {
                    subtype: Some("turn_aborted".to_owned()),
                    body: reason.to_owned(),
                },
                timestamp,
                None,
            );
        }
        // agent_message, task_started, task_complete, token_count: either
        // duplicated elsewhere (see module doc) or pure telemetry, not
        // conversation content.
        _ => {}
    }
}

fn parse_codex_response_item(
    payload: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    model: &Option<String>,
    seq: &mut u64,
    events: &mut Vec<TranscriptEvent>,
) {
    let Some(item_type) = payload.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    match item_type {
        "message" => parse_codex_message(payload, timestamp, model, seq, events),
        "reasoning" => parse_codex_reasoning(payload, timestamp, model, seq, events),
        "custom_tool_call" | "function_call" => parse_codex_tool_call(item_type, payload, timestamp, seq, events),
        "custom_tool_call_output" | "function_call_output" => parse_codex_tool_output(payload, timestamp, seq, events),
        _ => {}
    }
}

fn parse_codex_message(
    payload: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    model: &Option<String>,
    seq: &mut u64,
    events: &mut Vec<TranscriptEvent>,
) {
    // Only the assistant's own messages are taken from response_item; see
    // the module doc for why user/developer messages are skipped here.
    if payload.get("role").and_then(|v| v.as_str()) != Some("assistant") {
        return;
    }
    let Some(content) = payload.get("content").and_then(|v| v.as_array()) else {
        return;
    };
    // Shared with driver progress via `boss_engine_codex_rollout`.
    let text = extract_codex_text_blocks(content);
    if text.is_empty() {
        return;
    }
    push_codex_event(
        events,
        seq,
        TranscriptEventKind::AssistantText(text),
        timestamp,
        model.clone(),
    );
}

fn parse_codex_reasoning(
    payload: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    model: &Option<String>,
    seq: &mut u64,
    events: &mut Vec<TranscriptEvent>,
) {
    let Some(summary) = payload.get("summary").and_then(|v| v.as_array()) else {
        return;
    };
    let text = summary
        .iter()
        .filter_map(|block| block.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    // Reasoning summaries are frequently absent (encrypted-only content with
    // no visible summary); no text means nothing to show, not an error.
    if text.is_empty() {
        return;
    }
    push_codex_event(
        events,
        seq,
        TranscriptEventKind::Thinking(text),
        timestamp,
        model.clone(),
    );
}

fn parse_codex_tool_call(
    item_type: &str,
    payload: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    seq: &mut u64,
    events: &mut Vec<TranscriptEvent>,
) {
    // Shared reshape (exec/exec_command → Bash, argv coerce) with
    // `boss_engine_driver::codex::progress` via `boss_engine_codex_rollout`.
    let Some(call) = canonical_rollout_tool_call(item_type, payload) else {
        return;
    };
    push_codex_event(
        events,
        seq,
        TranscriptEventKind::ToolUse {
            name: call.tool_name,
            input: call.tool_input,
        },
        timestamp,
        None,
    );
}

fn parse_codex_tool_output(
    payload: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    seq: &mut u64,
    events: &mut Vec<TranscriptEvent>,
) {
    // Shared with the lower crate: plain string / content-block array / JSON
    // string `{"output","metadata.exit_code"}` (Codex shell tool dialect).
    let parsed = payload.get("output").map(canonical_rollout_tool_output).unwrap_or(
        boss_engine_codex_rollout::CanonicalRolloutToolOutput {
            body: String::new(),
            is_error: false,
        },
    );
    push_codex_event(
        events,
        seq,
        TranscriptEventKind::ToolResult {
            output: parsed.body,
            is_error: parsed.is_error,
        },
        timestamp,
        None,
    );
}

fn parse_one_value(value: &Value, seq: &mut u64) -> Vec<TranscriptEvent> {
    let Some(obj) = value.as_object() else {
        return Vec::new();
    };
    let Some(type_str) = obj.get("type").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    let timestamp = obj.get("timestamp").and_then(|v| v.as_str()).map(|s| s.to_owned());
    let model = obj.get("model").and_then(|v| v.as_str()).map(|s| s.to_owned());

    match type_str {
        "user" => parse_user_message(obj, timestamp, seq),
        "assistant" => parse_assistant_message(obj, timestamp, model, seq),
        "tool_result" => parse_tool_result(obj, timestamp, seq).into_iter().collect(),
        "system" => parse_system_event(obj, timestamp, seq).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// The turn content of a user/assistant entry, in whichever of the two
/// accepted dialects it arrived in (see the crate docs): Claude's
/// `message.content` envelope, or the canonical normalized shape's top-level
/// `content` / bare `text`.
///
/// `message.content` is checked first so a Claude entry is read exactly as it
/// always was; the top-level lookups only ever fire for an entry that has no
/// `message` envelope at all.
fn turn_content(obj: &serde_json::Map<String, Value>) -> Option<&Value> {
    obj.get("message")
        .and_then(|message| message.get("content"))
        .or_else(|| obj.get("content"))
        .or_else(|| obj.get("text"))
}

fn parse_user_message(
    obj: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    seq: &mut u64,
) -> Vec<TranscriptEvent> {
    let Some(content) = turn_content(obj) else {
        return Vec::new();
    };
    extract_text_blocks(content, "user", timestamp, None, seq)
}

fn parse_assistant_message(
    obj: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    model: Option<String>,
    seq: &mut u64,
) -> Vec<TranscriptEvent> {
    let Some(content) = turn_content(obj) else {
        return Vec::new();
    };
    let model = model.or_else(|| {
        obj.get("message")
            .and_then(|message| message.get("model"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
    });

    let mut events = Vec::new();
    if let Some(arr) = content.as_array() {
        for block in arr {
            let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
            match block_type {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        let s = *seq;
                        *seq += 1;
                        events.push(TranscriptEvent {
                            seq: s,
                            kind: TranscriptEventKind::AssistantText(text.to_owned()),
                            timestamp: timestamp.clone(),
                            model: model.clone(),
                        });
                    }
                }
                "thinking" => {
                    if let Some(thinking) = block.get("thinking").and_then(|v| v.as_str()) {
                        let s = *seq;
                        *seq += 1;
                        events.push(TranscriptEvent {
                            seq: s,
                            kind: TranscriptEventKind::Thinking(thinking.to_owned()),
                            timestamp: timestamp.clone(),
                            model: model.clone(),
                        });
                    }
                }
                "tool_use" => {
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_owned();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    let s = *seq;
                    *seq += 1;
                    events.push(TranscriptEvent {
                        seq: s,
                        kind: TranscriptEventKind::ToolUse { name, input },
                        timestamp: timestamp.clone(),
                        model: model.clone(),
                    });
                }
                _ => {}
            }
        }
    } else if let Some(text) = content.as_str() {
        let s = *seq;
        *seq += 1;
        events.push(TranscriptEvent {
            seq: s,
            kind: TranscriptEventKind::AssistantText(text.to_owned()),
            timestamp,
            model,
        });
    }
    events
}

fn extract_text_blocks(
    content: &Value,
    role: &str,
    timestamp: Option<String>,
    model: Option<String>,
    seq: &mut u64,
) -> Vec<TranscriptEvent> {
    let mut events = Vec::new();
    let make_kind: fn(String) -> TranscriptEventKind = if role == "user" {
        TranscriptEventKind::UserText
    } else {
        TranscriptEventKind::AssistantText
    };
    if let Some(arr) = content.as_array() {
        for block in arr {
            let bt = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
            if bt == "text"
                && let Some(text) = block.get("text").and_then(|v| v.as_str())
            {
                let s = *seq;
                *seq += 1;
                events.push(TranscriptEvent {
                    seq: s,
                    kind: make_kind(text.to_owned()),
                    timestamp: timestamp.clone(),
                    model: model.clone(),
                });
            }
        }
    } else if let Some(text) = content.as_str() {
        let s = *seq;
        *seq += 1;
        events.push(TranscriptEvent {
            seq: s,
            kind: make_kind(text.to_owned()),
            timestamp,
            model,
        });
    }
    events
}

fn parse_tool_result(
    obj: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    seq: &mut u64,
) -> Option<TranscriptEvent> {
    let output = if let Some(content) = obj.get("content") {
        if let Some(arr) = content.as_array() {
            arr.iter()
                .filter_map(|block| {
                    let bt = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                    if bt == "text" {
                        block.get("text").and_then(|v| v.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else if let Some(text) = content.as_str() {
            text.to_owned()
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Claude Code writes "isError" (camelCase); accept both spellings
    let is_error = obj
        .get("isError")
        .or_else(|| obj.get("is_error"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let s = *seq;
    *seq += 1;
    Some(TranscriptEvent {
        seq: s,
        kind: TranscriptEventKind::ToolResult { output, is_error },
        timestamp,
        model: None,
    })
}

fn parse_system_event(
    obj: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
    seq: &mut u64,
) -> Option<TranscriptEvent> {
    let subtype = obj.get("subtype").and_then(|v| v.as_str()).map(|s| s.to_owned());

    let body = match subtype.as_deref() {
        Some("pr-link") => {
            // Body is the raw PR URL
            obj.get("pr_url")
                .or_else(|| obj.get("url"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned()
        }
        _ => {
            // Body = JSON of all fields except type/subtype/timestamp/sessionId
            let mut body_obj = serde_json::Map::new();
            for (k, v) in obj {
                if k != "type" && k != "subtype" && k != "timestamp" && k != "sessionId" {
                    body_obj.insert(k.clone(), v.clone());
                }
            }
            if body_obj.is_empty() {
                String::new()
            } else {
                serde_json::to_string_pretty(&Value::Object(body_obj)).unwrap_or_default()
            }
        }
    };

    let s = *seq;
    *seq += 1;
    Some(TranscriptEvent {
        seq: s,
        kind: TranscriptEventKind::System { subtype, body },
        timestamp,
        model: None,
    })
}

// ── events_to_segments ────────────────────────────────────────────────────────

/// Convert normalized transcript events into renderable segments.
pub fn events_to_segments(events: &[TranscriptEvent], opts: &RenderOpts) -> Vec<TranscriptSegment> {
    events.iter().filter_map(|ev| event_to_segment(ev, opts)).collect()
}

fn event_to_segment(event: &TranscriptEvent, opts: &RenderOpts) -> Option<TranscriptSegment> {
    match &event.kind {
        TranscriptEventKind::UserText(text) => Some(
            TranscriptSegment::builder()
                .seq(event.seq)
                .role(SegmentRole::User)
                .label("User")
                .maybe_timestamp(event.timestamp.clone())
                .markdown(text.clone())
                .build(),
        ),

        TranscriptEventKind::AssistantText(text) => Some(
            TranscriptSegment::builder()
                .seq(event.seq)
                .role(SegmentRole::Assistant)
                .label("Assistant")
                .maybe_timestamp(event.timestamp.clone())
                .maybe_model(event.model.clone())
                .markdown(text.clone())
                .build(),
        ),

        TranscriptEventKind::Thinking(text) => {
            let markdown = blockquote(text);
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::Thinking)
                    .label("💭 Thinking")
                    .maybe_timestamp(event.timestamp.clone())
                    .maybe_model(event.model.clone())
                    .markdown(markdown)
                    .collapsible(true)
                    .default_collapsed(true)
                    .build(),
            )
        }

        TranscriptEventKind::ToolUse { name, input } => {
            if opts.hide_tools {
                return None;
            }
            let markdown = render_tool_use(name, input);
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::Tool)
                    .label(format!("⚙ {name}"))
                    .maybe_timestamp(event.timestamp.clone())
                    .markdown(markdown)
                    .build(),
            )
        }

        TranscriptEventKind::ToolResult { output, is_error } => {
            if opts.hide_tools {
                return None;
            }
            render_tool_result_segment(event, output, *is_error, opts)
        }

        TranscriptEventKind::System { subtype, body } => render_system_segment(event, subtype.as_deref(), body),
    }
}

fn render_tool_use(name: &str, input: &Value) -> String {
    match name {
        "Bash" => {
            let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            format!("```sh\n{command}\n```")
        }
        "Edit" => {
            let path = input
                .get("file_path")
                .or_else(|| input.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let old = input.get("old_string").and_then(|v| v.as_str());
            let new = input.get("new_string").and_then(|v| v.as_str());
            match (old, new) {
                (Some(old_str), Some(new_str)) => {
                    format!("**Edit** `{path}`\n\n**Replace:**\n```\n{old_str}\n```\n\n**With:**\n```\n{new_str}\n```")
                }
                _ => {
                    let json = serde_json::to_string_pretty(input).unwrap_or_default();
                    format!("**Edit** `{path}`\n\n```json\n{json}\n```")
                }
            }
        }
        "Write" => {
            let path = input
                .get("file_path")
                .or_else(|| input.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
            format!("**Write** `{path}`\n\n```\n{content}\n```")
        }
        _ => {
            let json = serde_json::to_string_pretty(input).unwrap_or_default();
            format!("```json\n{json}\n```")
        }
    }
}

fn render_tool_result_segment(
    event: &TranscriptEvent,
    output: &str,
    is_error: bool,
    opts: &RenderOpts,
) -> Option<TranscriptSegment> {
    let total_bytes = output.len();
    let (shown_output, truncated) = if total_bytes > opts.max_result_bytes {
        let shown_len = boss_engine_utils::string_clip::floor_char_boundary(output, opts.max_result_bytes);
        (
            &output[..shown_len],
            Some(TruncationInfo {
                shown_bytes: shown_len,
                total_bytes,
            }),
        )
    } else {
        (output, None)
    };

    let error_marker = if is_error { "❌ **Error**\n\n" } else { "" };
    let markdown = format!("{error_marker}```\n{shown_output}\n```");
    let large = truncated.is_some() || total_bytes > 1024;

    Some(
        TranscriptSegment::builder()
            .seq(event.seq)
            .role(SegmentRole::Tool)
            .label("↳ result")
            .maybe_timestamp(event.timestamp.clone())
            .markdown(markdown)
            .collapsible(large)
            .maybe_truncated(truncated)
            .build(),
    )
}

fn render_system_segment(event: &TranscriptEvent, subtype: Option<&str>, body: &str) -> Option<TranscriptSegment> {
    match subtype {
        Some("init") => None,
        Some("pr-link") => {
            let markdown = if body.starts_with("http") {
                format!("[🔗 View PR]({body})")
            } else {
                body.to_owned()
            };
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::System)
                    .label("🔗 PR")
                    .maybe_timestamp(event.timestamp.clone())
                    .markdown(markdown)
                    .build(),
            )
        }
        Some("stop_hook_summary") => {
            let markdown = if body.is_empty() {
                "> *(no summary)*".to_owned()
            } else {
                blockquote(body)
            };
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::System)
                    .label("stop_hook_summary")
                    .maybe_timestamp(event.timestamp.clone())
                    .markdown(markdown)
                    .build(),
            )
        }
        Some("turn_duration") => {
            let markdown = if body.is_empty() {
                String::new()
            } else {
                blockquote(body)
            };
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::System)
                    .label("turn_duration")
                    .maybe_timestamp(event.timestamp.clone())
                    .markdown(markdown)
                    .build(),
            )
        }
        Some(subtype_str) => {
            // Hook events, attachments, etc.
            let verbose = body.len() > 500;
            let markdown = render_body_as_markdown(body);
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::System)
                    .label(subtype_str.to_owned())
                    .maybe_timestamp(event.timestamp.clone())
                    .markdown(markdown)
                    .collapsible(verbose)
                    .build(),
            )
        }
        None => {
            let markdown = render_body_as_markdown(body);
            Some(
                TranscriptSegment::builder()
                    .seq(event.seq)
                    .role(SegmentRole::System)
                    .label("system")
                    .maybe_timestamp(event.timestamp.clone())
                    .markdown(markdown)
                    .build(),
            )
        }
    }
}

// ── segments_to_markdown ──────────────────────────────────────────────────────

/// Flatten segments into a single markdown document (for the CLI
/// `--format=markdown` path and the single-blob `MarkdownDocRef` source).
pub fn segments_to_markdown(segs: &[TranscriptSegment]) -> String {
    let mut out = String::new();
    for seg in segs {
        out.push_str(&format!("## {}\n\n", segment_header(seg)));
        out.push_str(&seg.markdown);
        if !seg.markdown.ends_with('\n') {
            out.push('\n');
        }
        if let Some(t) = &seg.truncated {
            out.push_str(&format!("\n*…showing {} of {} bytes*\n", t.shown_bytes, t.total_bytes));
        }
        out.push('\n');
    }
    out
}

fn segment_header(seg: &TranscriptSegment) -> String {
    let mut parts = vec![seg.label.clone()];
    if let Some(ts) = &seg.timestamp {
        parts.push(ts.clone());
    }
    if let Some(model) = &seg.model {
        parts.push(format!("*{model}*"));
    }
    parts.join(" · ")
}

// ── render_text (plain-text CLI renderer) ─────────────────────────────────────

/// Render transcript events as plain text for the CLI `agents transcript`
/// command (format=text).
pub fn render_text(events: &[TranscriptEvent], opts: &RenderOpts) -> String {
    let segs = events_to_segments(events, opts);
    let mut out = String::new();
    for seg in &segs {
        let header = segment_header(seg);
        out.push_str(&format!("=== {header} ===\n"));
        out.push_str(&strip_markdown(&seg.markdown));
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if let Some(t) = &seg.truncated {
            out.push_str(&format!("[…showing {} of {} bytes]\n", t.shown_bytes, t.total_bytes));
        }
        out.push('\n');
    }
    out
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn blockquote(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    text.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n")
}

fn render_body_as_markdown(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    let trimmed = body.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        format!("```json\n{trimmed}\n```")
    } else {
        blockquote(trimmed)
    }
}

fn strip_markdown(md: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if let Some(stripped) = trimmed.strip_prefix("> ") {
            out.push_str(stripped);
        } else if let Some(stripped) = trimmed.strip_prefix('>') {
            out.push_str(stripped);
        } else if trimmed.starts_with("**") && trimmed.ends_with("**") {
            out.push_str(&trimmed[2..trimmed.len() - 2]);
        } else {
            out.push_str(trimmed);
        }
        out.push('\n');
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_transcript ──────────────────────────────────────────────────────

    #[test]
    fn parses_user_text_message() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Hello!"}]},"timestamp":"2024-01-01T00:00:00.000Z"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::UserText(t) => assert_eq!(t, "Hello!"),
            other => panic!("unexpected kind: {other:?}"),
        }
        assert_eq!(events[0].timestamp.as_deref(), Some("2024-01-01T00:00:00.000Z"));
    }

    #[test]
    fn parses_assistant_text_message() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hi there!"}]},"model":"claude-sonnet-4-6","timestamp":"2024-01-01T00:00:01.000Z"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::AssistantText(t) => assert_eq!(t, "Hi there!"),
            other => panic!("unexpected kind: {other:?}"),
        }
        assert_eq!(events[0].model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn parses_canonical_assistant_entry_without_a_message_envelope() {
        // The shape every non-Claude driver's `normalize_transcript_entry`
        // produces: `content` at the top level, no `message` wrapper. Reading
        // only the Claude envelope is what made the Stop-boundary marker scans
        // blind on those drivers.
        let jsonl =
            r#"{"type":"assistant","content":[{"type":"text","text":"[blocked] reason=\"lock file is unwritable\""}]}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1, "canonical assistant entry must parse: {events:?}");
        match &events[0].kind {
            TranscriptEventKind::AssistantText(t) => {
                assert!(t.starts_with("[blocked] reason="), "got {t}");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_canonical_user_entry_with_bare_text() {
        let jsonl = r#"{"type":"user","text":"run the build"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::UserText(t) => assert_eq!(t, "run the build"),
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_canonical_assistant_tool_use_without_a_message_envelope() {
        let jsonl =
            r#"{"type":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"jj diff"}}]}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::ToolUse { name, input } => {
                assert_eq!(name, "Bash");
                assert_eq!(input["command"], "jj diff");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parse_transcript_values_matches_a_raw_text_parse() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"go"}]}}"#,
            "\n",
            r#"{"type":"assistant","content":[{"type":"text","text":"done"}]}"#,
            "\n",
        );
        let values = jsonl
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap());
        let from_values = parse_transcript_values(values);
        let from_text = parse_transcript(jsonl);
        assert_eq!(from_values.len(), from_text.len());
        assert_eq!(from_values.len(), 2);
        // `seq` numbering must be continuous across entries either way.
        assert_eq!(from_values[0].seq, 0);
        assert_eq!(from_values[1].seq, 1);
    }

    #[test]
    fn message_envelope_still_wins_over_a_top_level_content_field() {
        // Defensive: if an entry somehow carries both, the Claude envelope is
        // authoritative — the top-level lookup is a fallback, not an override.
        let jsonl = r#"{"type":"assistant","content":[{"type":"text","text":"fallback"}],"message":{"content":[{"type":"text","text":"envelope"}]}}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::AssistantText(t) => assert_eq!(t, "envelope"),
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_thinking_block() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me reason about this."},{"type":"text","text":"Answer."}]},"model":"claude-sonnet-4-6"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].kind, TranscriptEventKind::Thinking(_)));
        assert!(matches!(events[1].kind, TranscriptEventKind::AssistantText(_)));
    }

    #[test]
    fn parses_tool_use_bash() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la"}}]}}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::ToolUse { name, input } => {
                assert_eq!(name, "Bash");
                assert_eq!(input.get("command").and_then(|v| v.as_str()), Some("ls -la"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_tool_use_edit() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/foo.rs","old_string":"let x","new_string":"let y"}}]}}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::ToolUse { name, .. } => assert_eq!(name, "Edit"),
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_tool_result_ok() {
        let jsonl = r#"{"type":"tool_result","toolUseId":"t1","content":[{"type":"text","text":"file.txt\ndir/"}],"isError":false,"timestamp":"2024-01-01T00:00:03.000Z"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::ToolResult { output, is_error } => {
                assert!(output.contains("file.txt"));
                assert!(!is_error);
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_tool_result_error() {
        let jsonl = r#"{"type":"tool_result","toolUseId":"t1","content":[{"type":"text","text":"command not found"}],"isError":true}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::ToolResult { is_error, .. } => assert!(is_error),
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_system_init_event() {
        let jsonl = r#"{"type":"system","subtype":"init","cwd":"/workspace","timestamp":"2024-01-01T00:00:00.000Z","model":"claude-sonnet-4-6"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::System { subtype, .. } => {
                assert_eq!(subtype.as_deref(), Some("init"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_system_pr_link() {
        let jsonl = r#"{"type":"system","subtype":"pr-link","pr_url":"https://github.com/foo/bar/pull/1","timestamp":"2024-01-01T00:00:10.000Z"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::System { subtype, body } => {
                assert_eq!(subtype.as_deref(), Some("pr-link"));
                assert_eq!(body, "https://github.com/foo/bar/pull/1");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_system_stop_hook_summary() {
        let jsonl = r#"{"type":"system","subtype":"stop_hook_summary","summary":"Task complete.","timestamp":"2024-01-01T00:01:00.000Z"}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::System { subtype, .. } => {
                assert_eq!(subtype.as_deref(), Some("stop_hook_summary"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parses_system_turn_duration() {
        let jsonl = r#"{"type":"system","subtype":"turn_duration","duration_ms":1234}"#;
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            TranscriptEventKind::System { subtype, .. } => {
                assert_eq!(subtype.as_deref(), Some("turn_duration"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn skips_malformed_lines() {
        let jsonl = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n{not valid json\n{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"reply\"}]}}";
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn skips_unknown_type() {
        let jsonl = r#"{"type":"unknown_type","data":"whatever"}"#;
        let events = parse_transcript(jsonl);
        assert!(events.is_empty());
    }

    #[test]
    fn skips_empty_lines() {
        let jsonl = "\n\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n\n";
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn seq_increments_across_events() {
        let jsonl = concat!(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"a\"}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"b\"}]}}\n",
            "{\"type\":\"tool_result\",\"content\":[{\"type\":\"text\",\"text\":\"c\"}],\"isError\":false}"
        );
        let events = parse_transcript(jsonl);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].seq, 0);
        assert_eq!(events[1].seq, 1);
        assert_eq!(events[2].seq, 2);
    }

    // ── events_to_segments ────────────────────────────────────────────────────

    #[test]
    fn user_text_segment() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::UserText("Hello".to_owned()),
            timestamp: Some("2024-01-01T00:00:00.000Z".to_owned()),
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].role, SegmentRole::User);
        assert_eq!(segs[0].label, "User");
        assert_eq!(segs[0].markdown, "Hello");
        assert!(!segs[0].collapsible);
    }

    #[test]
    fn assistant_text_segment_carries_model() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::AssistantText("Hi".to_owned()),
            timestamp: None,
            model: Some("claude-sonnet-4-6".to_owned()),
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs[0].role, SegmentRole::Assistant);
        assert_eq!(segs[0].model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn thinking_segment_is_collapsible_and_collapsed() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::Thinking("my thoughts".to_owned()),
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs[0].role, SegmentRole::Thinking);
        assert!(segs[0].collapsible);
        assert!(segs[0].default_collapsed);
        assert!(segs[0].markdown.contains("> my thoughts"));
    }

    #[test]
    fn bash_tool_use_renders_sh_fence() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolUse {
                name: "Bash".to_owned(),
                input: serde_json::json!({"command": "echo hello"}),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs[0].role, SegmentRole::Tool);
        assert!(segs[0].markdown.contains("```sh\necho hello\n```"));
    }

    #[test]
    fn edit_tool_use_renders_path_and_diff() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolUse {
                name: "Edit".to_owned(),
                input: serde_json::json!({
                    "file_path": "/src/main.rs",
                    "old_string": "let x = 1;",
                    "new_string": "let x = 2;"
                }),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        let md = &segs[0].markdown;
        assert!(md.contains("`/src/main.rs`"), "got: {md}");
        assert!(md.contains("let x = 1;"), "got: {md}");
        assert!(md.contains("let x = 2;"), "got: {md}");
    }

    #[test]
    fn write_tool_use_renders_path_and_content() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolUse {
                name: "Write".to_owned(),
                input: serde_json::json!({
                    "file_path": "/out.txt",
                    "content": "hello world"
                }),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        let md = &segs[0].markdown;
        assert!(md.contains("`/out.txt`"), "got: {md}");
        assert!(md.contains("hello world"), "got: {md}");
    }

    #[test]
    fn unknown_tool_use_renders_json_fence() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolUse {
                name: "Read".to_owned(),
                input: serde_json::json!({"file_path": "/foo.rs"}),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert!(segs[0].markdown.contains("```json"));
    }

    #[test]
    fn tool_result_ok_not_collapsed_when_small() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolResult {
                output: "ok".to_owned(),
                is_error: false,
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs[0].role, SegmentRole::Tool);
        assert_eq!(segs[0].label, "↳ result");
        assert!(!segs[0].collapsible);
        assert!(segs[0].truncated.is_none());
    }

    #[test]
    fn tool_result_error_adds_marker() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolResult {
                output: "not found".to_owned(),
                is_error: true,
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert!(segs[0].markdown.contains("❌"));
    }

    #[test]
    fn tool_result_truncated_when_over_limit() {
        let big = "x".repeat(20_000);
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::ToolResult {
                output: big.clone(),
                is_error: false,
            },
            timestamp: None,
            model: None,
        }];
        let opts = RenderOpts {
            max_result_bytes: 1024,
            ..RenderOpts::default()
        };
        let segs = events_to_segments(&events, &opts);
        assert!(segs[0].collapsible);
        let t = segs[0].truncated.as_ref().expect("truncated should be set");
        assert_eq!(t.shown_bytes, 1024);
        assert_eq!(t.total_bytes, 20_000);
    }

    #[test]
    fn system_init_is_skipped() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::System {
                subtype: Some("init".to_owned()),
                body: String::new(),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert!(segs.is_empty(), "init events should be filtered");
    }

    #[test]
    fn system_pr_link_renders_markdown_link() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::System {
                subtype: Some("pr-link".to_owned()),
                body: "https://github.com/foo/bar/pull/42".to_owned(),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs[0].label, "🔗 PR");
        assert!(segs[0].markdown.contains("[🔗 View PR]"));
        assert!(segs[0].markdown.contains("https://github.com/foo/bar/pull/42"));
    }

    #[test]
    fn system_stop_hook_summary_renders_blockquote() {
        let events = vec![TranscriptEvent {
            seq: 0,
            kind: TranscriptEventKind::System {
                subtype: Some("stop_hook_summary".to_owned()),
                body: "All done.".to_owned(),
            },
            timestamp: None,
            model: None,
        }];
        let segs = events_to_segments(&events, &RenderOpts::default());
        assert_eq!(segs[0].label, "stop_hook_summary");
        assert!(segs[0].markdown.starts_with("> "));
    }

    // ── segments_to_markdown ──────────────────────────────────────────────────

    #[test]
    fn segments_to_markdown_produces_h2_headers() {
        let segs = vec![
            TranscriptSegment::builder()
                .seq(0)
                .role(SegmentRole::User)
                .label("User")
                .markdown("Hello")
                .build(),
        ];
        let md = segments_to_markdown(&segs);
        assert!(md.contains("## User\n\nHello"), "got: {md}");
    }

    #[test]
    fn segments_to_markdown_includes_timestamp_in_header() {
        let segs = vec![
            TranscriptSegment::builder()
                .seq(0)
                .role(SegmentRole::Assistant)
                .label("Assistant")
                .timestamp("2024-01-01T00:00:01Z")
                .model("claude-sonnet-4-6")
                .markdown("Reply")
                .build(),
        ];
        let md = segments_to_markdown(&segs);
        assert!(md.contains("2024-01-01T00:00:01Z"));
        assert!(md.contains("claude-sonnet-4-6"));
    }

    #[test]
    fn segments_to_markdown_adds_truncation_note() {
        let segs = vec![
            TranscriptSegment::builder()
                .seq(0)
                .role(SegmentRole::Tool)
                .label("↳ result")
                .markdown("```\nshort\n```")
                .collapsible(true)
                .maybe_truncated(Some(TruncationInfo {
                    shown_bytes: 100,
                    total_bytes: 5000,
                }))
                .build(),
        ];
        let md = segments_to_markdown(&segs);
        assert!(md.contains("showing 100 of 5000 bytes"), "got: {md}");
    }

    // ── render_text ───────────────────────────────────────────────────────────

    #[test]
    fn render_text_produces_plain_text() {
        let events = parse_transcript(concat!(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}"
        ));
        let text = render_text(&events, &RenderOpts::default());
        assert!(text.contains("=== User ==="), "got: {text}");
        assert!(text.contains("hi"), "got: {text}");
        assert!(text.contains("=== Assistant ==="), "got: {text}");
        assert!(text.contains("hello"), "got: {text}");
    }

    #[test]
    fn render_text_hide_tools_omits_tool_segments() {
        let jsonl = concat!(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"run ls\"}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{\"command\":\"ls\"}}]}}\n",
            "{\"type\":\"tool_result\",\"toolUseId\":\"t1\",\"content\":[{\"type\":\"text\",\"text\":\"file.txt\"}],\"isError\":false}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}"
        );
        let events = parse_transcript(jsonl);
        let opts = RenderOpts {
            hide_tools: true,
            ..RenderOpts::default()
        };
        let text = render_text(&events, &opts);
        assert!(!text.contains("Bash"), "tool_use should be hidden, got: {text}");
        assert!(!text.contains("file.txt"), "tool_result should be hidden, got: {text}");
        assert!(text.contains("run ls"), "got: {text}");
        assert!(text.contains("done"), "got: {text}");
    }

    // ── Codex rollout parsing ─────────────────────────────────────────────────

    const CODEX_ROLLOUT_SAMPLE: &str = include_str!("testdata/codex_rollout_sample.jsonl");

    #[test]
    fn detects_codex_rollout_schema_and_extracts_turns() {
        let events = parse_transcript(CODEX_ROLLOUT_SAMPLE);
        assert!(
            !events.is_empty(),
            "expected Codex rollout records to parse into events"
        );

        let user_texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match &e.kind {
                TranscriptEventKind::UserText(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(user_texts, vec!["run: sleep 10; reply with exactly: q7-tail-done"]);

        let assistant_texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match &e.kind {
                TranscriptEventKind::AssistantText(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(assistant_texts, vec!["q7-tail-done"]);

        let tool_uses: Vec<(&str, &Value)> = events
            .iter()
            .filter_map(|e| match &e.kind {
                TranscriptEventKind::ToolUse { name, input } => Some((name.as_str(), input)),
                _ => None,
            })
            .collect();
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].0, "Bash");
        let command = tool_uses[0].1.get("command").and_then(|v| v.as_str()).unwrap_or("");
        assert!(command.contains("sleep 10"), "got: {command}");

        let tool_results: Vec<(&str, bool)> = events
            .iter()
            .filter_map(|e| match &e.kind {
                TranscriptEventKind::ToolResult { output, is_error } => Some((output.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 1);
        assert!(
            tool_results[0].0.contains("Script completed"),
            "got: {}",
            tool_results[0].0
        );
        assert!(!tool_results[0].1);

        // session_meta, world_state, turn_context, task_started/task_complete,
        // token_count are structural/telemetry and must not surface as turns.
        for ev in &events {
            if let TranscriptEventKind::System { subtype, .. } = &ev.kind {
                panic!("unexpected system event in a clean turn: {subtype:?}");
            }
        }
    }

    #[test]
    fn codex_rollout_renders_via_markdown_and_text() {
        let events = parse_transcript(CODEX_ROLLOUT_SAMPLE);
        let segments = events_to_segments(&events, &RenderOpts::default());
        let markdown = segments_to_markdown(&segments);
        assert!(
            markdown.contains("run: sleep 10; reply with exactly: q7-tail-done"),
            "got: {markdown}"
        );
        assert!(markdown.contains("q7-tail-done"), "got: {markdown}");
        assert!(markdown.contains("```sh"), "got: {markdown}");

        let text = render_text(&events, &RenderOpts::default());
        assert!(
            text.contains("run: sleep 10; reply with exactly: q7-tail-done"),
            "got: {text}"
        );
        assert!(text.contains("q7-tail-done"), "got: {text}");
    }

    #[test]
    fn codex_rollout_no_tools_hides_tool_segments() {
        let events = parse_transcript(CODEX_ROLLOUT_SAMPLE);
        let opts = RenderOpts {
            hide_tools: true,
            ..RenderOpts::default()
        };
        let text = render_text(&events, &opts);
        assert!(!text.contains("⚙"), "tool_use should be hidden, got: {text}");
        assert!(
            !text.contains("Script completed"),
            "tool_result should be hidden, got: {text}"
        );
        assert!(
            text.contains("run: sleep 10; reply with exactly: q7-tail-done"),
            "got: {text}"
        );
        assert!(text.contains("q7-tail-done"), "got: {text}");
    }

    #[test]
    fn parse_transcript_checked_succeeds_for_claude_code() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#;
        let events = parse_transcript_checked(jsonl).expect("Claude Code schema should be recognised");
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn parse_transcript_checked_succeeds_for_codex_rollout() {
        let events = parse_transcript_checked(CODEX_ROLLOUT_SAMPLE).expect("Codex rollout schema should be recognised");
        assert!(!events.is_empty());
    }

    #[test]
    fn parse_transcript_checked_errors_on_unrecognized_schema() {
        let jsonl = r#"{"type":"something_else","data":1}"#;
        let err = parse_transcript_checked(jsonl).expect_err("unknown schema should be reported, not swallowed");
        assert!(!err.message.is_empty());
    }

    #[test]
    fn parse_transcript_checked_ok_empty_for_genuinely_empty_content() {
        let events = parse_transcript_checked("").expect("empty content is not an unrecognised schema");
        assert!(events.is_empty());
    }

    #[test]
    fn parse_transcript_falls_back_to_empty_on_unrecognized_schema() {
        let jsonl = r#"{"type":"something_else","data":1}"#;
        assert!(parse_transcript(jsonl).is_empty());
    }

    #[test]
    fn unrecognized_schema_message_mentions_tail_window_when_types_present() {
        let jsonl = r#"{"type":"something_else","data":1}"#;
        let err = parse_transcript_checked(jsonl).expect_err("unknown type");
        assert!(
            err.message.contains("unrecognised type") || err.message.contains("try a larger --lines"),
            "got: {}",
            err.message
        );
    }

    // ── Codex parse arms (table-driven) ───────────────────────────────────────

    #[test]
    fn codex_parse_arms_table() {
        // Each case is one or more rollout JSONL lines and a predicate over events.
        struct Case {
            name: &'static str,
            jsonl: &'static str,
            check: fn(&[TranscriptEvent]),
        }

        let cases = [
            Case {
                name: "reasoning summary → Thinking",
                jsonl: concat!(
                    r#"{"timestamp":"t0","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"plan the fix"}]}}"#,
                    "\n"
                ),
                check: |events| {
                    assert_eq!(events.len(), 1, "expected one thinking event");
                    match &events[0].kind {
                        TranscriptEventKind::Thinking(t) => assert_eq!(t, "plan the fix"),
                        other => panic!("expected Thinking, got {other:?}"),
                    }
                },
            },
            Case {
                name: "turn_aborted → System",
                jsonl: concat!(
                    r#"{"timestamp":"t0","type":"event_msg","payload":{"type":"turn_aborted","reason":"user_interrupt"}}"#,
                    "\n"
                ),
                check: |events| {
                    assert_eq!(events.len(), 1);
                    match &events[0].kind {
                        TranscriptEventKind::System { subtype, body } => {
                            assert_eq!(subtype.as_deref(), Some("turn_aborted"));
                            assert_eq!(body, "user_interrupt");
                        }
                        other => panic!("expected System, got {other:?}"),
                    }
                },
            },
            Case {
                name: "function_call exec_command string args → Bash",
                jsonl: concat!(
                    r#"{"timestamp":"t0","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"echo hi\"}"}}"#,
                    "\n"
                ),
                check: |events| {
                    assert_eq!(events.len(), 1);
                    match &events[0].kind {
                        TranscriptEventKind::ToolUse { name, input } => {
                            assert_eq!(name, "Bash");
                            assert_eq!(input.get("command").and_then(|v| v.as_str()), Some("echo hi"));
                        }
                        other => panic!("expected ToolUse, got {other:?}"),
                    }
                },
            },
            Case {
                name: "exec cmd array → Bash command string for markdown",
                jsonl: concat!(
                    r#"{"timestamp":"t0","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"c2","input":{"cmd":["echo","hi"]}}}"#,
                    "\n"
                ),
                check: |events| {
                    assert_eq!(events.len(), 1);
                    let segs = events_to_segments(events, &RenderOpts::default());
                    assert_eq!(segs.len(), 1);
                    assert!(
                        segs[0].markdown.contains("echo hi"),
                        "expected command in fence, got: {}",
                        segs[0].markdown
                    );
                    assert!(segs[0].markdown.contains("```sh"), "got: {}", segs[0].markdown);
                },
            },
            Case {
                name: "turn_context.model attaches to subsequent assistant",
                jsonl: concat!(
                    r#"{"timestamp":"t0","type":"turn_context","payload":{"model":"gpt-test-model"}}"#,
                    "\n",
                    r#"{"timestamp":"t1","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello model"}]}}"#,
                    "\n"
                ),
                check: |events| {
                    assert_eq!(events.len(), 1);
                    match &events[0].kind {
                        TranscriptEventKind::AssistantText(t) => assert_eq!(t, "hello model"),
                        other => panic!("expected AssistantText, got {other:?}"),
                    }
                    assert_eq!(events[0].model.as_deref(), Some("gpt-test-model"));
                },
            },
            Case {
                name: "function_call_output JSON string with exit_code → is_error",
                jsonl: concat!(
                    r#"{"timestamp":"t0","type":"response_item","payload":{"type":"function_call_output","call_id":"c3","output":"{\"output\":\"boom\\n\",\"metadata\":{\"exit_code\":1}}"}}"#,
                    "\n"
                ),
                check: |events| {
                    assert_eq!(events.len(), 1);
                    match &events[0].kind {
                        TranscriptEventKind::ToolResult { output, is_error } => {
                            assert_eq!(output, "boom\n");
                            assert!(*is_error);
                        }
                        other => panic!("expected ToolResult, got {other:?}"),
                    }
                },
            },
            Case {
                name: "function_call_output plain string stays non-error",
                jsonl: concat!(
                    r#"{"timestamp":"t0","type":"response_item","payload":{"type":"function_call_output","call_id":"c4","output":"https://example.com/pr/1\n"}}"#,
                    "\n"
                ),
                check: |events| {
                    assert_eq!(events.len(), 1);
                    match &events[0].kind {
                        TranscriptEventKind::ToolResult { output, is_error } => {
                            assert!(output.contains("example.com"));
                            assert!(!*is_error);
                        }
                        other => panic!("expected ToolResult, got {other:?}"),
                    }
                },
            },
        ];

        for case in cases {
            let events = parse_transcript(case.jsonl);
            (case.check)(&events);
            // Also exercise markdown for the Bash cases so empty fences fail loudly.
            if case.name.contains("Bash") || case.name.contains("cmd array") {
                let md = segments_to_markdown(&events_to_segments(&events, &RenderOpts::default()));
                assert!(md.contains("echo hi"), "{}: markdown missing command: {md}", case.name);
            }
        }
    }

    // ── char-boundary truncation ──────────────────────────────────────────────

    #[test]
    fn truncation_len_respects_char_boundaries() {
        use boss_engine_utils::string_clip::floor_char_boundary;
        // "é" is 2 bytes (0xC3 0xA9). Truncating at byte 1 would be invalid.
        let s = "aé";
        assert_eq!(s.len(), 3); // 'a'=1, 'é'=2
        assert_eq!(floor_char_boundary(s, 2), 1); // can't split 'é', so stop at 'a'
        assert_eq!(floor_char_boundary(s, 3), 3);
        assert_eq!(floor_char_boundary(s, 10), 3);
    }
}
