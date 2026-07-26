//! Codex progress and transcript normalisation.
//!
//! Codex exposes two JSONL dialects which are deliberately parsed separately:
//!
//! - `codex exec --json` stdout uses `thread.*`, `turn.*`, and `item.*`
//!   envelopes. Those records drive [`WorkerEvent`].
//! - rollout transcripts use `session_meta`, `event_msg`, and
//!   `response_item`. Those records are only reshaped for transcript
//!   redaction/status today; they are not a second progress ingress.
//!
//! Keeping the parsers distinct prevents rollout-only events such as
//! `event_msg.turn_aborted` from being invented on the stdout channel.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use boss_protocol::{NormalizeError, SessionStartSource, StopReason, WorkerEvent};
use serde_json::{Map, Value, json};

/// Per-driver progress state.
///
/// A `DriverRegistry` is constructed for each stdout-ingress run, so this
/// state belongs to one stream in production. Codex puts `thread_id` only on
/// `thread.started`; every later stdout envelope inherits the current id.
#[derive(Default)]
pub(super) struct CodexProgressNormalizer {
    state: Mutex<CodexProgressState>,
}

#[derive(Default)]
struct CodexProgressState {
    current_thread_id: Option<String>,
    transcript_paths: HashMap<String, String>,
    rollout_exec_calls: HashMap<String, Value>,
}

/// Threads observed by any stdout-ingress process in this engine.
///
/// `codex exec resume` is a new process, and production creates a fresh
/// registry/driver for that new stdout stream. Known-thread identity therefore
/// has to outlive one [`CodexProgressNormalizer`] instance even though the
/// current-thread pointer remains stream-local.
static KNOWN_THREAD_IDS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Parsed stdout dialect. This is intentionally not reused for rollout lines.
enum StdoutEnvelope<'a> {
    ThreadStarted {
        thread_id: &'a str,
    },
    TurnStarted,
    TurnCompleted,
    CommandStarted {
        command: &'a Value,
    },
    CommandCompleted {
        command: &'a Value,
        output: &'a Value,
    },
    OperationalWarning {
        message: &'a str,
    },
    Unknown {
        envelope_type: &'a str,
        item_type: Option<&'a str>,
    },
}

/// Parsed rollout dialect. This is intentionally not reused for stdout.
enum RolloutEnvelope {
    SessionMeta,
    EventMessage { event_type: String },
    ExecCall { call_id: String, input: Value },
    ToolCallOutput { call_id: String, output: Value },
    OtherResponseItem { item_type: String },
    Unknown { record_type: String },
}

impl CodexProgressNormalizer {
    pub(super) fn normalize_stdout(&self, raw: &Value) -> Result<WorkerEvent, NormalizeError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| NormalizeError::Malformed("expected Codex stdout JSON object".into()))?;
        let envelope = parse_stdout_envelope(obj)?;

        match envelope {
            StdoutEnvelope::ThreadStarted { thread_id } => {
                let mut state = self.lock_state()?;
                let continuation = !known_thread_ids()
                    .lock()
                    .map_err(|_| NormalizeError::Malformed("Codex known-thread lock poisoned".into()))?
                    .insert(thread_id.to_owned());
                // `exec resume` re-emits thread.started with the same id. Keep
                // all known-thread/transcript state and merely make that
                // existing thread current again.
                state.current_thread_id = Some(thread_id.to_owned());
                Ok(WorkerEvent::SessionStart {
                    session_id: thread_id.to_owned(),
                    source: if continuation {
                        SessionStartSource::Resume
                    } else {
                        SessionStartSource::Startup
                    },
                })
            }
            StdoutEnvelope::Unknown {
                envelope_type,
                item_type,
            } => {
                tracing::debug!(
                    envelope_type,
                    item_type = item_type.unwrap_or("<none>"),
                    "codex stdout: ignoring additive event variant"
                );
                Err(NormalizeError::UnknownEvent(match item_type {
                    Some(item_type) => format!("{envelope_type}/{item_type}"),
                    None => envelope_type.to_owned(),
                }))
            }
            other => {
                let session_id = self.current_thread_id()?;
                Ok(match other {
                    StdoutEnvelope::TurnStarted => WorkerEvent::UserPromptSubmit {
                        session_id,
                        prompt: String::new(),
                    },
                    StdoutEnvelope::TurnCompleted => WorkerEvent::Stop {
                        session_id,
                        stop_hook_active: false,
                        stop_reason: StopReason::Completed,
                    },
                    StdoutEnvelope::CommandStarted { command } => WorkerEvent::PreToolUse {
                        session_id,
                        tool_name: "Bash".to_owned(),
                        tool_input: command_input(command),
                    },
                    StdoutEnvelope::CommandCompleted { command, output } => WorkerEvent::PostToolUse {
                        session_id,
                        tool_name: "Bash".to_owned(),
                        tool_input: command_input(command),
                        // Keep aggregated_output as a bare string. The shared
                        // PR-URL capture seam explicitly supports this shape.
                        tool_response: output.clone(),
                    },
                    StdoutEnvelope::OperationalWarning { message } => WorkerEvent::Notification {
                        session_id,
                        message: message.to_owned(),
                    },
                    StdoutEnvelope::ThreadStarted { .. } | StdoutEnvelope::Unknown { .. } => {
                        unreachable!("handled above")
                    }
                })
            }
        }
    }

    /// Discover the rollout for the thread named by `raw`, or by the current
    /// sticky thread when this envelope omits `thread_id`.
    pub(super) fn transcript_path(&self, raw: &Value, homes_root: &Path) -> Option<String> {
        let raw_thread_id = raw.get("thread_id").and_then(Value::as_str).map(str::to_owned);
        let thread_id = {
            let state = self.state.lock().ok()?;
            raw_thread_id.or_else(|| state.current_thread_id.clone())?
        };

        if let Some(cached) = self.state.lock().ok()?.transcript_paths.get(&thread_id).cloned() {
            return Some(cached);
        }

        let path = discover_rollout_path(homes_root, &thread_id)?;
        let path = path.to_string_lossy().into_owned();
        self.state.lock().ok()?.transcript_paths.insert(thread_id, path.clone());
        Some(path)
    }

    /// Reshape one rollout transcript record for the shared live-status
    /// redactor. Rollout parsing remains separate from stdout progress
    /// parsing; this method never emits a [`WorkerEvent`].
    pub(super) fn normalize_rollout(&self, mut raw: Value) -> Value {
        let parsed = match raw.as_object().and_then(parse_rollout_envelope) {
            Some(parsed) => parsed,
            None => {
                tracing::debug!("codex rollout: ignoring malformed/non-object transcript record");
                return raw;
            }
        };

        match parsed {
            RolloutEnvelope::SessionMeta => raw,
            RolloutEnvelope::EventMessage { event_type } => {
                // Keep the original rollout shape, but expose the nested event
                // type to the transcript status/redaction layer. In
                // particular, turn_aborted remains rollout-only.
                insert_top_level(&mut raw, "codex_rollout_event", Value::String(event_type));
                raw
            }
            RolloutEnvelope::ExecCall { call_id, input } => {
                if let Ok(mut state) = self.state.lock() {
                    state.rollout_exec_calls.insert(call_id, input.clone());
                }
                insert_top_level(&mut raw, "tool_name", Value::String("Bash".to_owned()));
                insert_top_level(&mut raw, "tool_input", command_input(&input));
                raw
            }
            RolloutEnvelope::ToolCallOutput { call_id, output } => {
                let known_exec = self
                    .state
                    .lock()
                    .ok()
                    .and_then(|state| state.rollout_exec_calls.get(&call_id).cloned());
                if known_exec.is_some() {
                    insert_top_level(&mut raw, "tool_name", Value::String("Bash".to_owned()));
                    insert_top_level(&mut raw, "tool_response", output);
                } else {
                    tracing::debug!(
                        call_id,
                        "codex rollout: leaving unmatched custom_tool_call_output unchanged"
                    );
                }
                raw
            }
            RolloutEnvelope::OtherResponseItem { item_type } => {
                tracing::debug!(item_type, "codex rollout: leaving non-exec response item unchanged");
                raw
            }
            RolloutEnvelope::Unknown { record_type } => {
                tracing::debug!(record_type, "codex rollout: ignoring additive record variant");
                raw
            }
        }
    }

    fn current_thread_id(&self) -> Result<String, NormalizeError> {
        self.lock_state()?
            .current_thread_id
            .clone()
            .ok_or(NormalizeError::MissingField("thread_id"))
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, CodexProgressState>, NormalizeError> {
        self.state
            .lock()
            .map_err(|_| NormalizeError::Malformed("Codex progress state lock poisoned".into()))
    }
}

fn known_thread_ids() -> &'static Mutex<HashSet<String>> {
    KNOWN_THREAD_IDS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn parse_stdout_envelope(obj: &Map<String, Value>) -> Result<StdoutEnvelope<'_>, NormalizeError> {
    let envelope_type = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or(NormalizeError::MissingField("type"))?;

    match envelope_type {
        "thread.started" => {
            let thread_id = obj
                .get("thread_id")
                .and_then(Value::as_str)
                .ok_or(NormalizeError::MissingField("thread_id"))?;
            Ok(StdoutEnvelope::ThreadStarted { thread_id })
        }
        "turn.started" => Ok(StdoutEnvelope::TurnStarted),
        "turn.completed" => Ok(StdoutEnvelope::TurnCompleted),
        "item.started" | "item.completed" => {
            let item = obj
                .get("item")
                .and_then(Value::as_object)
                .ok_or(NormalizeError::MissingField("item"))?;
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .ok_or(NormalizeError::MissingField("item.type"))?;
            match (envelope_type, item_type) {
                ("item.started", "command_execution") => {
                    let command = item
                        .get("command")
                        .ok_or(NormalizeError::MissingField("item.command"))?;
                    Ok(StdoutEnvelope::CommandStarted { command })
                }
                ("item.completed", "command_execution") => {
                    let command = item
                        .get("command")
                        .ok_or(NormalizeError::MissingField("item.command"))?;
                    let output = item
                        .get("aggregated_output")
                        .ok_or(NormalizeError::MissingField("item.aggregated_output"))?;
                    Ok(StdoutEnvelope::CommandCompleted { command, output })
                }
                ("item.completed", "error") => Ok(StdoutEnvelope::OperationalWarning {
                    message: item
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Codex reported an operational warning"),
                }),
                _ => Ok(StdoutEnvelope::Unknown {
                    envelope_type,
                    item_type: Some(item_type),
                }),
            }
        }
        // `turn_aborted` is intentionally absent: it is a rollout
        // event_msg variant, never a codex exec --json stdout envelope.
        _ => Ok(StdoutEnvelope::Unknown {
            envelope_type,
            item_type: None,
        }),
    }
}

fn parse_rollout_envelope(obj: &Map<String, Value>) -> Option<RolloutEnvelope> {
    let record_type = obj.get("type")?.as_str()?;
    let payload = obj.get("payload")?.as_object()?;
    match record_type {
        "session_meta" => Some(RolloutEnvelope::SessionMeta),
        "event_msg" => Some(RolloutEnvelope::EventMessage {
            event_type: payload.get("type")?.as_str()?.to_owned(),
        }),
        "response_item" => {
            let item_type = payload.get("type")?.as_str()?;
            match item_type {
                "custom_tool_call" if payload.get("name").and_then(Value::as_str) == Some("exec") => {
                    Some(RolloutEnvelope::ExecCall {
                        call_id: payload.get("call_id")?.as_str()?.to_owned(),
                        input: payload.get("input").cloned().unwrap_or(Value::Null),
                    })
                }
                "custom_tool_call_output" => Some(RolloutEnvelope::ToolCallOutput {
                    call_id: payload.get("call_id")?.as_str()?.to_owned(),
                    output: payload.get("output").cloned().unwrap_or(Value::Null),
                }),
                _ => Some(RolloutEnvelope::OtherResponseItem {
                    item_type: item_type.to_owned(),
                }),
            }
        }
        _ => Some(RolloutEnvelope::Unknown {
            record_type: record_type.to_owned(),
        }),
    }
}

fn command_input(command: &Value) -> Value {
    match command {
        Value::String(command) => json!({ "command": command }),
        other => other.clone(),
    }
}

fn insert_top_level(raw: &mut Value, key: &str, value: Value) {
    if let Some(obj) = raw.as_object_mut() {
        obj.insert(key.to_owned(), value);
    }
}

/// Find the newest rollout whose filename ends in this thread id.
///
/// Codex embeds a local timestamp before the id, so pure construction is not
/// possible. Symlinked directories are skipped to keep discovery contained
/// under the Boss-owned homes root.
fn discover_rollout_path(homes_root: &Path, thread_id: &str) -> Option<PathBuf> {
    let expected_suffix = format!("-{thread_id}.jsonl");
    let mut stack = vec![homes_root.to_path_buf()];
    let mut matches = Vec::new();

    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(&expected_suffix) {
                matches.push((name.into_owned(), entry.path()));
            }
        }
    }

    // Lexical ordering includes the ISO timestamp in the filename and gives a
    // deterministic newest result if an interrupted resume left duplicates.
    matches.sort_by(|left, right| left.0.cmp(&right.0));
    matches.pop().map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn normalizer() -> CodexProgressNormalizer {
        CodexProgressNormalizer::default()
    }

    #[test]
    fn stdout_turn_and_command_execution_map_to_worker_events() {
        let normalizer = normalizer();
        assert_eq!(
            normalizer
                .normalize_stdout(&json!({"type":"thread.started","thread_id":"thread-1"}))
                .unwrap(),
            WorkerEvent::SessionStart {
                session_id: "thread-1".into(),
                source: SessionStartSource::Startup,
            }
        );
        assert!(matches!(
            normalizer.normalize_stdout(&json!({"type":"turn.started"})).unwrap(),
            WorkerEvent::UserPromptSubmit { session_id, .. } if session_id == "thread-1"
        ));
        assert_eq!(
            normalizer
                .normalize_stdout(&json!({
                    "type":"item.started",
                    "item":{"id":"item_0","type":"command_execution","command":"echo hi"}
                }))
                .unwrap(),
            WorkerEvent::PreToolUse {
                session_id: "thread-1".into(),
                tool_name: "Bash".into(),
                tool_input: json!({"command":"echo hi"}),
            }
        );
        assert_eq!(
            normalizer
                .normalize_stdout(&json!({
                    "type":"item.completed",
                    "item":{
                        "id":"item_0",
                        "type":"command_execution",
                        "command":"echo hi",
                        "aggregated_output":"hi\n",
                        "exit_code":0,
                        "status":"completed"
                    }
                }))
                .unwrap(),
            WorkerEvent::PostToolUse {
                session_id: "thread-1".into(),
                tool_name: "Bash".into(),
                tool_input: json!({"command":"echo hi"}),
                tool_response: json!("hi\n"),
            }
        );
        assert_eq!(
            normalizer
                .normalize_stdout(&json!({"type":"turn.completed","usage":{"future_counter":7}}))
                .unwrap(),
            WorkerEvent::Stop {
                session_id: "thread-1".into(),
                stop_hook_active: false,
                stop_reason: StopReason::Completed,
            }
        );
    }

    #[test]
    fn repeated_thread_started_for_known_id_is_resume_without_state_reset() {
        let normalizer = normalizer();
        normalizer
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"same-thread"}))
            .unwrap();
        let repeated = normalizer
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"same-thread"}))
            .unwrap();
        assert_eq!(
            repeated,
            WorkerEvent::SessionStart {
                session_id: "same-thread".into(),
                source: SessionStartSource::Resume,
            }
        );
        assert!(matches!(
            normalizer.normalize_stdout(&json!({"type":"turn.started"})).unwrap(),
            WorkerEvent::UserPromptSubmit { session_id, .. } if session_id == "same-thread"
        ));
    }

    #[test]
    fn resumed_process_recognizes_thread_seen_by_prior_normalizer_instance() {
        let first_process = normalizer();
        first_process
            .normalize_stdout(&json!({
                "type":"thread.started",
                "thread_id":"cross-process-resume-thread"
            }))
            .unwrap();

        let resumed_process = normalizer();
        assert_eq!(
            resumed_process
                .normalize_stdout(&json!({
                    "type":"thread.started",
                    "thread_id":"cross-process-resume-thread"
                }))
                .unwrap(),
            WorkerEvent::SessionStart {
                session_id: "cross-process-resume-thread".into(),
                source: SessionStartSource::Resume,
            }
        );
        assert!(matches!(
            resumed_process
                .normalize_stdout(&json!({"type":"turn.started"}))
                .unwrap(),
            WorkerEvent::UserPromptSubmit { session_id, .. }
                if session_id == "cross-process-resume-thread"
        ));
    }

    #[test]
    fn operational_error_item_is_non_terminal_notification() {
        let normalizer = normalizer();
        normalizer
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"thread-warning"}))
            .unwrap();
        let event = normalizer
            .normalize_stdout(&json!({
                "type":"item.completed",
                "item":{"id":"item_0","type":"error","message":"hook trust bypassed"}
            }))
            .unwrap();
        assert_eq!(
            event,
            WorkerEvent::Notification {
                session_id: "thread-warning".into(),
                message: "hook trust bypassed".into(),
            }
        );
        assert!(!matches!(event, WorkerEvent::Stop { .. }));
    }

    #[test]
    fn additive_variants_are_tolerated_and_do_not_require_session_state() {
        let normalizer = normalizer();
        for raw in [
            json!({"type":"thread.future","new_field":true}),
            json!({"type":"item.completed","item":{"id":"item_0","type":"extension","payload":{}}}),
            json!({"type":"item.started","item":{"id":"item_0","type":"dynamic_tool_call"}}),
            json!({"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"done"}}),
        ] {
            assert!(matches!(
                normalizer.normalize_stdout(&raw),
                Err(NormalizeError::UnknownEvent(_))
            ));
        }
    }

    #[test]
    fn stdout_turn_aborted_is_not_synthesized() {
        let normalizer = normalizer();
        assert!(matches!(
            normalizer.normalize_stdout(&json!({
                "type":"turn_aborted",
                "turn_id":"turn-1",
                "reason":"interrupted"
            })),
            Err(NormalizeError::UnknownEvent(_))
        ));
    }

    #[test]
    fn rollout_parser_is_distinct_and_marks_turn_aborted_only_there() {
        let normalizer = normalizer();
        let normalized = normalizer.normalize_rollout(json!({
            "timestamp":"2026-07-26T00:00:00Z",
            "type":"event_msg",
            "payload":{"type":"turn_aborted","turn_id":"turn-1","reason":"interrupted"}
        }));
        assert_eq!(normalized["type"], "event_msg");
        assert_eq!(normalized["codex_rollout_event"], "turn_aborted");
        assert_eq!(normalized["payload"]["reason"], "interrupted");
    }

    #[test]
    fn rollout_exec_call_and_output_get_canonical_tool_fields() {
        let normalizer = normalizer();
        let call = normalizer.normalize_rollout(json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "call_id":"call-1",
                "name":"exec",
                "input":"echo rollout"
            }
        }));
        assert_eq!(call["tool_name"], "Bash");
        assert_eq!(call["tool_input"], json!({"command":"echo rollout"}));

        let output = normalizer.normalize_rollout(json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":"call-1",
                "output":[{"type":"input_text","text":"rollout\n"}]
            }
        }));
        assert_eq!(output["tool_name"], "Bash");
        assert_eq!(
            output["tool_response"],
            json!([{"type":"input_text","text":"rollout\n"}])
        );
    }

    #[test]
    fn transcript_discovery_globs_timestamped_rollout_by_thread_id() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("run-1/sessions/2026/07/26");
        fs::create_dir_all(&sessions).unwrap();
        let expected = sessions.join("rollout-2026-07-26T03-48-52-thread-lookup.jsonl");
        fs::write(&expected, "{}\n").unwrap();
        fs::write(sessions.join("rollout-2026-07-26T03-48-53-other-thread.jsonl"), "{}\n").unwrap();

        let normalizer = normalizer();
        normalizer
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"thread-lookup"}))
            .unwrap();
        assert_eq!(
            normalizer.transcript_path(&json!({"type":"turn.started"}), tmp.path()),
            Some(expected.to_string_lossy().into_owned())
        );
    }
}
