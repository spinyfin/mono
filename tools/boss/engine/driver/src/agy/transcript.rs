//! Antigravity native-transcript dialect → canonical redactable entries.
//!
//! `agy` writes one JSON row per *step* to
//! `$HOME/.gemini/antigravity-cli/brain/<cid>/.system_generated/logs/transcript_full.jsonl`
//! (every hook payload stamps that path as `transcriptPath`). Rows observed on
//! 1.5.9 — the committed captures in `testdata/` are verbatim:
//!
//! ```json
//! {"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"<USER_REQUEST>\n…\n</USER_REQUEST>\n<ADDITIONAL_METADATA>…"}
//! {"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","model":"gemini-3.8-flash-low","input_tokens":9413,"cache_read_tokens":0,"output_tokens":47,"tool_calls":[{"name":"run_command","args":{"CommandLine":"echo hi","WaitMsBeforeAsync":5000}}]}
//! {"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","content":"…Command exited with code 0.\n\nCommand output:\nhi\r\n"}
//! {"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"done"}
//! ```
//!
//! The live-status summariser, the `[blocked]` / `[effort-escalation]` marker
//! scans and the structured-output prose fallback all read the canonical
//! shape the other drivers produce (`assistant` entries carrying `text` /
//! `tool_use` blocks, `tool_result`, `user`, `system`). This module is the
//! agy-specific half of that contract. Like Grok's and Codex's, it is
//! deliberately not shared with any other dialect.
//!
//! ## Decisions that follow from what the studies measured
//!
//! - **A tool's result row has no call id.** `tool_calls[]` entries carry a
//!   name and `args` only, and the `GENERIC` row that answers one is joined to
//!   it by position. [`AgyTranscriptSession`] therefore keeps a FIFO of the
//!   calls from the *current* model turn and resolves result rows against it;
//!   a new `PLANNER_RESPONSE`, `USER_INPUT` or `CHECKPOINT` empties it. That
//!   boundary matters: an Esc-cancelled tool step is simply **absent** from
//!   the transcript, so a call that never got a result must not be paired
//!   with whatever row happens to arrive after the next prompt.
//! - **Status is not completion.** A cancelled generation is recorded `DONE`,
//!   and a backgrounded command's row is `RUNNING` with the *launch*
//!   acknowledgement as its content (the command's real outcome arrives later
//!   as a `SYSTEM_MESSAGE`). This module therefore never synthesises a
//!   "turn completed" entry, never turns a `RUNNING` row into a
//!   `tool_result`, and never claims a pending call resolved without a row
//!   that resolves it.
//! - **A command's exit code is not a tool error.** `Command exited with code
//!   7.` arrives on a `DONE` row. `is_error` mirrors the row's own `ERROR`
//!   status only (a refused or failed *step*); reading an exit status out of
//!   prose would be the `CommandOutcomeObservation` capability the driver
//!   deliberately does not declare.
//! - **Token counts are optional.** 1.5.x planner rows carry `input_tokens`,
//!   `cache_read_tokens` and `output_tokens`; 1.1.12 rows carry none. They are
//!   consumed from the raw transcript by the engine cost extractor; this
//!   display normalizer leaves accounting to that reader. Missing counts stay unknown.
//! - **Only `PLANNER_RESPONSE` is worker prose.** Everything a command printed
//!   (`GENERIC`), a background completion notice (`SYSTEM_MESSAGE`) and a
//!   compaction summary (`CHECKPOINT`) is reshaped to a non-assistant entry, so
//!   a marker-shaped line in a tool's output can never be mistaken for the
//!   worker's own `[blocked]` declaration.
//! - **`ERROR_MESSAGE` rows are the halting-error shape.** The CLI's step enum
//!   names that type; no halting error has been captured yet, so the shape is
//!   inferred, not measured. It is reshaped to the flagged `system` entry the
//!   shared extractor ([`AgyDriver::extract_error_from_transcript`]) reads, and
//!   stays inert if the real type turns out to differ.
//!
//! [`AgyDriver::extract_error_from_transcript`]: crate::AgentDriver::extract_error_from_transcript

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use serde_json::{Map, Value, json};

use super::progress::{canonical_tool_input, tool_name_mapped};
use crate::TranscriptSessionNormalizer;

/// A `tool_calls[]` entry awaiting its result row.
struct PendingToolCall {
    name: String,
    input: Value,
}

/// Per-tail correlation state for one `agy` transcript.
///
/// A registry driver may be shared by concurrent readers; this type is not —
/// the engine constructs one per tail ([`crate::AgentDriver::transcript_session`]),
/// so one worker's in-flight calls can never leak into another's. Within one
/// tail the queue is emptied at every model/user/checkpoint row, so it never
/// holds more than the calls of a single model turn.
#[derive(Default)]
pub(super) struct AgyTranscriptSession {
    pending: VecDeque<PendingToolCall>,
}

impl AgyTranscriptSession {
    /// Reshape one native row. Rows that are already canonical, not objects,
    /// or of a native step type this dialect does not know pass through
    /// unchanged — an additive step type must never abort a tail.
    pub(super) fn normalize(&mut self, raw: Value) -> Value {
        self.reshape(&raw).unwrap_or(raw)
    }

    fn reshape(&mut self, raw: &Value) -> Option<Value> {
        let row = raw.as_object()?;
        let step_type = row.get("type")?.as_str()?;
        Some(match step_type {
            "USER_INPUT" => {
                self.pending.clear();
                stamped(
                    json!({"type": "user", "text": user_request(text_of(row, "content"))}),
                    row,
                )
            }
            "PLANNER_RESPONSE" => self.planner_response(row),
            "CHECKPOINT" => {
                self.pending.clear();
                system(row, "checkpoint", text_of(row, "content"))
            }
            "SYSTEM_MESSAGE" => system(row, "system_message", text_of(row, "content")),
            "ERROR_MESSAGE" => api_error(row),
            "GENERIC" => self.tool_step(row).unwrap_or_else(|| {
                tracing::debug!("agy transcript: GENERIC step with no pending tool call");
                system(row, "generic_step", text_of(row, "content"))
            }),
            other if is_native_step_type(other) => match (matches!(status_of(row), "DONE" | "ERROR")
                && (!text_of(row, "content").trim().is_empty() || !text_of(row, "error").trim().is_empty()))
            .then(|| self.tool_step(row))
            .flatten()
            {
                Some(entry) => entry,
                None => {
                    tracing::debug!(step_type = other, "agy transcript: ignoring additive native step type");
                    return None;
                }
            },
            // Lower-case or otherwise non-native: already canonical.
            _ => return None,
        })
    }

    fn planner_response(&mut self, row: &Map<String, Value>) -> Value {
        // A new model turn: any call from the previous one will never get a
        // result row (its step was cancelled and is absent from the file).
        self.pending.clear();
        let mut blocks = Vec::new();
        let prose = text_of(row, "content");
        if !prose.is_empty() {
            blocks.push(json!({"type": "text", "text": prose}));
        }
        for call in row.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let name = tool_name_mapped(call.get("name").and_then(Value::as_str).unwrap_or(""));
            let input = call.get("args").map(canonical_tool_input).unwrap_or(Value::Null);
            blocks.push(json!({"type": "tool_use", "name": &name, "input": &input}));
            self.pending.push_back(PendingToolCall { name, input });
        }
        if blocks.is_empty() {
            return system(row, "planner_response", "");
        }
        let mut entry = stamped(json!({"type": "assistant", "content": blocks}), row);
        if let Some(model) = row.get("model").and_then(Value::as_str) {
            entry["model"] = Value::String(model.to_owned());
        }
        entry
    }

    /// Resolve a model-sourced step row against the oldest pending call.
    ///
    /// `None` means the row is not a tool result: nothing is pending, or the
    /// row is sourced from the user or the system.
    fn tool_step(&mut self, row: &Map<String, Value>) -> Option<Value> {
        if !is_model_sourced(row) {
            return None;
        }
        let call = self.pending.pop_front()?;
        if status_of(row) == "RUNNING" {
            // Backgrounded: this row only acknowledges the launch. The call is
            // accounted for, but no result exists yet.
            return Some(system(
                row,
                "tool_running",
                &format!("{} is running as a background task", call.name),
            ));
        }
        let content = match text_of(row, "content") {
            "" => text_of(row, "error"),
            content => content,
        };
        Some(stamped(
            json!({
                "type": "tool_result",
                "content": content,
                "is_error": status_of(row) == "ERROR",
                "tool_name": call.name,
                "tool_input": call.input,
            }),
            row,
        ))
    }
}

impl TranscriptSessionNormalizer for AgyTranscriptSession {
    fn normalize_transcript_entry(&mut self, raw: Value) -> Value {
        self.normalize(raw)
    }
}

/// Reshape one native row for a caller that keeps no cross-record state
/// (direct callers and single-record fixtures). The live-status tail and the
/// post-hoc readers use [`crate::AgentDriver::transcript_session`], whose
/// [`AgyTranscriptSession`] pairs each tool call with its result; an isolated
/// call here sees only the call half of that pair.
pub(super) fn normalize_transcript_entry(raw: Value) -> Value {
    AgyTranscriptSession::default().normalize(raw)
}

/// Native step types are `UPPER_SNAKE`; canonical entry types are lower-case.
fn is_native_step_type(step_type: &str) -> bool {
    !step_type.is_empty() && step_type.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
}

/// A missing `source` (older rows) is treated as model-sourced; an explicit
/// `USER_EXPLICIT` or `SYSTEM` is not a tool's result.
fn is_model_sourced(row: &Map<String, Value>) -> bool {
    row.get("source")
        .and_then(Value::as_str)
        .is_none_or(|source| source == "MODEL")
}

fn status_of(row: &Map<String, Value>) -> &str {
    row.get("status").and_then(Value::as_str).unwrap_or("")
}

fn text_of<'a>(row: &'a Map<String, Value>, key: &str) -> &'a str {
    row.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The typed prompt, without the CLI's `<USER_REQUEST>` envelope and the
/// metadata block it appends.
fn user_request(content: &str) -> &str {
    content
        .strip_prefix("<USER_REQUEST>\n")
        .and_then(|rest| rest.split_once("\n</USER_REQUEST>"))
        .map_or(content, |(request, _)| request)
}

/// Carry the row's `created_at` onto the canonical entry as `timestamp`.
fn stamped(mut entry: Value, row: &Map<String, Value>) -> Value {
    if let Some(created_at) = row.get("created_at").and_then(Value::as_str) {
        entry["timestamp"] = Value::String(created_at.to_owned());
    }
    entry
}

/// Non-prose lifecycle filler. Tagged `system` so a marker scan or the
/// summariser never mistakes it for the worker's own words.
fn system(row: &Map<String, Value>, subtype: &str, message: &str) -> Value {
    let mut entry = json!({"type": "system", "subtype": subtype});
    if !message.is_empty() {
        entry["message"] = Value::String(message.to_owned());
    }
    stamped(entry, row)
}

/// A halting error, flagged the way the shared extractor reads it
/// (`isApiErrorMessage` plus text blocks).
fn api_error(row: &Map<String, Value>) -> Value {
    let text = match text_of(row, "error") {
        "" => text_of(row, "content"),
        error => error,
    };
    let text = if text.is_empty() {
        "agy reported an error step"
    } else {
        text
    };
    stamped(
        json!({
            "type": "system",
            "subtype": "api_error",
            "isApiErrorMessage": true,
            "content": [{"type": "text", "text": text}],
        }),
        row,
    )
}

const MAX_TRANSCRIPT_SCAN_BYTES: u64 = 64 * 1024 * 1024;

fn extract_running_task_id(content: &str) -> Option<&str> {
    let rest = content.split("task id:").nth(1)?;
    let id = rest.split_whitespace().next()?;
    (!id.is_empty()).then_some(id)
}

fn extract_completed_task_id(content: &str) -> Option<&str> {
    if let Some(rest) = content.split("sender=").nth(1)
        && let Some(id) = rest.split_whitespace().next()
        && !id.is_empty()
    {
        return Some(id);
    }
    if let Some(rest) = content.split("Task id \"").nth(1)
        && let Some(id) = rest.split('"').next()
        && !id.is_empty()
    {
        return Some(id);
    }
    None
}

/// Whether `reader` shows at least one `agy` background task (`GENERIC`
/// `RUNNING` / `tool_running`) that has not yet been closed by a matching
/// completion `SYSTEM_MESSAGE` / `system_message`.
pub(super) fn reader_has_live_background_task(reader: impl BufRead) -> bool {
    let mut active_task_ids: HashSet<String> = HashSet::new();
    let mut anonymous_running: usize = 0;

    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(row) = raw.as_object() else {
            continue;
        };
        let Some(step_type) = row.get("type").and_then(Value::as_str) else {
            continue;
        };
        match step_type {
            "GENERIC" if is_model_sourced(row) && status_of(row) == "RUNNING" => {
                let content = text_of(row, "content");
                if let Some(id) = extract_running_task_id(content) {
                    active_task_ids.insert(id.to_owned());
                } else {
                    anonymous_running += 1;
                }
            }
            "SYSTEM_MESSAGE" => {
                let content = text_of(row, "content");
                let removed = extract_completed_task_id(content).is_some_and(|id| active_task_ids.remove(id));
                if !removed && content.contains("finished with result:") {
                    anonymous_running = anonymous_running.saturating_sub(1);
                }
            }
            "system" => match row.get("subtype").and_then(Value::as_str) {
                Some("tool_running") => anonymous_running += 1,
                Some("system_message") => anonymous_running = anonymous_running.saturating_sub(1),
                _ => {}
            },
            _ => {}
        }
    }

    !active_task_ids.is_empty() || anonymous_running > 0
}

/// Check whether an `agy` transcript on disk currently has a background task
/// still running (a `GENERIC` `RUNNING` / `tool_running` row without a later
/// completion `SYSTEM_MESSAGE`).
pub fn transcript_has_live_background_task(path: &Path, containment_root: Option<&Path>) -> bool {
    if let Some(root) = containment_root {
        let (Ok(canon_root), Ok(canon_path)) = (std::fs::canonicalize(root), std::fs::canonicalize(path)) else {
            return false;
        };
        if !canon_path.starts_with(&canon_root) {
            return false;
        }
    }
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    reader_has_live_background_task(BufReader::new(file.take(MAX_TRANSCRIPT_SCAN_BYTES)))
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod tests;
