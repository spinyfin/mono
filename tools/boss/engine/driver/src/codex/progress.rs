//! Codex progress and transcript normalisation.
//!
//! Codex exposes two JSONL dialects which are deliberately parsed separately:
//!
//! - `codex exec --json` stdout uses `thread.*`, `turn.*`, and `item.*`
//!   envelopes. One [`CodexProgressSession`] owns correlation for one reader.
//! - rollout transcripts use `session_meta`, `event_msg`, and
//!   `response_item`. Those records are reshaped into the canonical
//!   user/assistant/tool records consumed by live-status rendering.
//!
//! Keeping the parsers distinct prevents rollout-only events such as
//! `event_msg.turn_aborted` from being invented on the stdout channel.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use boss_protocol::{NormalizeError, SessionStartSource, StopReason, WorkerEvent};
use serde_json::{Map, Value, json};

use crate::ProgressSessionNormalizer;

const THREAD_ID_MARKER: &str = ".boss-thread-id";

/// Mutable state owned by one stdout reader.
///
/// `current_thread_id` never lives on the registry's shared driver.
/// `codex_home` is the exact run home derived from the reader's run id;
/// transcript discovery cannot escape into a sibling run.
pub(super) struct CodexProgressSession {
    current_thread_id: Option<String>,
    codex_home: Option<PathBuf>,
}

impl CodexProgressSession {
    pub(super) fn new(codex_home: Option<PathBuf>) -> Self {
        Self {
            current_thread_id: None,
            codex_home,
        }
    }

    fn classify_thread_start(&mut self, thread_id: &str) -> SessionStartSource {
        // Keep only the current identity in memory. The durable marker below
        // restores that one lifecycle identity after an engine restart
        // without an unbounded process-global set of every thread ever seen.
        if self.current_thread_id.as_deref() == Some(thread_id) {
            return SessionStartSource::Resume;
        }

        let Some(codex_home) = self.codex_home.as_deref() else {
            return SessionStartSource::Startup;
        };
        match claim_persisted_thread_id(codex_home, thread_id) {
            Ok(true) => SessionStartSource::Resume,
            Ok(false) => SessionStartSource::Startup,
            Err(err) => {
                // Progress remains usable if the marker cannot be written.
                // The warning makes the resulting loss of restart-aware
                // resume classification explicit.
                tracing::warn!(
                    codex_home = %codex_home.display(),
                    %err,
                    "codex stdout: could not persist thread identity"
                );
                SessionStartSource::Startup
            }
        }
    }

    fn normalize_stdout(&mut self, raw: &Value) -> Result<WorkerEvent, NormalizeError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| NormalizeError::Malformed("expected Codex stdout JSON object".into()))?;
        let envelope = parse_stdout_envelope(obj)?;

        match envelope {
            StdoutEnvelope::ThreadStarted { thread_id } => {
                let source = self.classify_thread_start(thread_id);
                // A repeated thread.started from `exec resume` makes the same
                // thread current again without clearing any per-stream state.
                self.current_thread_id = Some(thread_id.to_owned());
                Ok(WorkerEvent::SessionStart {
                    session_id: thread_id.to_owned(),
                    source,
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
                let session_id = self
                    .current_thread_id
                    .clone()
                    .ok_or(NormalizeError::MissingField("thread_id"))?;
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
                        tool_input: json!({ "command": command }),
                    },
                    StdoutEnvelope::CommandCompleted { command, output } => WorkerEvent::PostToolUse {
                        session_id,
                        tool_name: "Bash".to_owned(),
                        tool_input: json!({ "command": command }),
                        // Keep aggregated_output as a bare string. The shared
                        // PR-URL capture seam explicitly supports this shape.
                        tool_response: Value::String(output.to_owned()),
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

    fn transcript_path(&self, raw: &Value) -> Option<String> {
        let thread_id = raw
            .get("thread_id")
            .and_then(Value::as_str)
            .or(self.current_thread_id.as_deref())?;
        let codex_home = self.codex_home.as_deref()?;
        discover_rollout_path(codex_home, thread_id).map(|path| path.to_string_lossy().into_owned())
    }
}

impl ProgressSessionNormalizer for CodexProgressSession {
    fn normalize_progress_event(&mut self, raw: &Value) -> Result<WorkerEvent, NormalizeError> {
        self.normalize_stdout(raw)
    }

    fn transcript_path_for_session(&mut self, raw: &Value) -> Option<String> {
        self.transcript_path(raw)
    }
}

/// Parsed stdout dialect. This is intentionally not reused for rollout lines.
enum StdoutEnvelope<'a> {
    ThreadStarted {
        thread_id: &'a str,
    },
    TurnStarted,
    TurnCompleted,
    CommandStarted {
        command: &'a str,
    },
    CommandCompleted {
        command: &'a str,
        output: &'a str,
    },
    OperationalWarning {
        message: &'a str,
    },
    Unknown {
        envelope_type: &'a str,
        item_type: Option<&'a str>,
    },
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
                        .and_then(Value::as_str)
                        .ok_or(NormalizeError::MissingField("item.command"))?;
                    Ok(StdoutEnvelope::CommandStarted { command })
                }
                ("item.completed", "command_execution") => {
                    let command = item
                        .get("command")
                        .and_then(Value::as_str)
                        .ok_or(NormalizeError::MissingField("item.command"))?;
                    let output = item
                        .get("aggregated_output")
                        .and_then(Value::as_str)
                        .ok_or(NormalizeError::MissingField("item.aggregated_output"))?;
                    Ok(StdoutEnvelope::CommandCompleted { command, output })
                }
                ("item.completed", "error") => {
                    let message = item
                        .get("message")
                        .and_then(Value::as_str)
                        .ok_or(NormalizeError::MissingField("item.message"))?;
                    Ok(StdoutEnvelope::OperationalWarning { message })
                }
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

/// Claim the one persisted thread identity for this run.
///
/// Returns `true` when the marker already named `thread_id` (resume) and
/// `false` when this call created/replaced it (startup). The file is bounded
/// to one id per run, survives engine restarts, and is deleted with the
/// Boss-owned CODEX_HOME during normal teardown.
fn claim_persisted_thread_id(codex_home: &Path, thread_id: &str) -> std::io::Result<bool> {
    let marker = codex_home.join(THREAD_ID_MARKER);
    match OpenOptions::new().write(true).create_new(true).open(&marker) {
        Ok(mut file) => {
            file.write_all(thread_id.as_bytes())?;
            file.sync_all()?;
            Ok(false)
        }
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            let persisted = fs::read_to_string(&marker)?;
            if persisted == thread_id {
                return Ok(true);
            }
            // A new thread under the same run is a startup and replaces the
            // bounded marker. This is not the resume case.
            let mut file = OpenOptions::new().write(true).truncate(true).open(marker)?;
            file.write_all(thread_id.as_bytes())?;
            file.sync_all()?;
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

/// Find a rollout only within this run's canonical `sessions` subtree.
///
/// Codex embeds a local timestamp before the thread id, so pure construction
/// is impossible. Symlinked entries are skipped, and both the sessions root
/// and candidate are canonicalized and containment-checked again immediately
/// before return to catch path replacement during traversal.
fn discover_rollout_path(codex_home: &Path, thread_id: &str) -> Option<PathBuf> {
    discover_rollout_path_after_scan(codex_home, thread_id, || {})
}

fn discover_rollout_path_after_scan(codex_home: &Path, thread_id: &str, after_scan: impl FnOnce()) -> Option<PathBuf> {
    let canonical_home = fs::canonicalize(codex_home).ok()?;
    let sessions_path = codex_home.join("sessions");
    if fs::symlink_metadata(&sessions_path).ok()?.file_type().is_symlink() {
        return None;
    }
    let canonical_sessions = fs::canonicalize(&sessions_path).ok()?;
    if canonical_sessions == canonical_home || !canonical_sessions.starts_with(&canonical_home) {
        return None;
    }

    let expected_suffix = format!("-{thread_id}.jsonl");
    let mut stack = vec![canonical_sessions.clone()];
    let mut matches = Vec::new();

    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                let canonical_dir = match fs::canonicalize(&path) {
                    Ok(path) if path.starts_with(&canonical_sessions) => path,
                    _ => continue,
                };
                stack.push(canonical_dir);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("rollout-") || !name.ends_with(&expected_suffix) {
                continue;
            }
            let canonical_candidate = match fs::canonicalize(&path) {
                Ok(path) if path.starts_with(&canonical_sessions) => path,
                _ => continue,
            };
            matches.push((name.into_owned(), canonical_candidate));
        }
    }

    matches.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let (_, candidate) = matches.pop()?;

    // Re-check both identities after traversal. A directory/file swapped to a
    // symlink between the first check and now is rejected.
    after_scan();
    let sessions_now = fs::canonicalize(&sessions_path).ok()?;
    let candidate_now = fs::canonicalize(&candidate).ok()?;
    if sessions_now != canonical_sessions
        || candidate_now != candidate
        || !candidate_now.starts_with(&canonical_sessions)
        || fs::symlink_metadata(&candidate).ok()?.file_type().is_symlink()
    {
        return None;
    }
    Some(candidate)
}

/// Parsed rollout dialect. This is intentionally not reused for stdout.
enum RolloutEnvelope {
    SessionMeta,
    EventMessage {
        event_type: String,
        payload: Map<String, Value>,
    },
    ResponseItem {
        item_type: String,
        payload: Map<String, Value>,
    },
    Unknown {
        record_type: String,
    },
}

fn parse_rollout_envelope(obj: &Map<String, Value>) -> Option<RolloutEnvelope> {
    let record_type = obj.get("type")?.as_str()?.to_owned();
    let payload = obj.get("payload")?.as_object()?.clone();
    match record_type.as_str() {
        "session_meta" => Some(RolloutEnvelope::SessionMeta),
        "event_msg" => Some(RolloutEnvelope::EventMessage {
            event_type: payload.get("type")?.as_str()?.to_owned(),
            payload,
        }),
        "response_item" => Some(RolloutEnvelope::ResponseItem {
            item_type: payload.get("type")?.as_str()?.to_owned(),
            payload,
        }),
        _ => Some(RolloutEnvelope::Unknown { record_type }),
    }
}

/// Reshape one rollout transcript record for the shared live-status pipeline.
///
/// This is stateless: output records are renderable on their own, so
/// concurrent transcript tails can share a driver without call-id maps.
pub(super) fn normalize_rollout(raw: Value) -> Value {
    let parsed = match raw.as_object().and_then(parse_rollout_envelope) {
        Some(parsed) => parsed,
        None => {
            tracing::debug!("codex rollout: ignoring malformed/non-object transcript record");
            return raw;
        }
    };

    match parsed {
        RolloutEnvelope::SessionMeta => json!({"type":"system"}),
        RolloutEnvelope::EventMessage { event_type, payload } => normalize_rollout_event_message(&event_type, &payload)
            .unwrap_or_else(|| {
                tracing::debug!(event_type, "codex rollout: ignoring additive event_msg variant");
                raw
            }),
        RolloutEnvelope::ResponseItem { item_type, payload } => normalize_rollout_response_item(&item_type, &payload)
            .unwrap_or_else(|| {
                tracing::debug!(item_type, "codex rollout: ignoring additive response_item variant");
                raw
            }),
        RolloutEnvelope::Unknown { record_type } => {
            tracing::debug!(record_type, "codex rollout: ignoring additive record variant");
            raw
        }
    }
}

fn normalize_rollout_event_message(event_type: &str, payload: &Map<String, Value>) -> Option<Value> {
    match event_type {
        "agent_message" => Some(assistant_text(payload.get("message")?.as_str()?)),
        "user_message" => Some(json!({
            "type":"user",
            "text": payload.get("message")?.as_str()?,
        })),
        "turn_aborted" => {
            let reason = payload.get("reason").and_then(Value::as_str).unwrap_or("interrupted");
            Some(assistant_text(&format!("turn aborted: {reason}")))
        }
        "task_started" => Some(assistant_text("turn started")),
        "task_complete" => {
            let message = payload
                .get("last_agent_message")
                .and_then(Value::as_str)
                .unwrap_or("turn completed");
            Some(assistant_text(message))
        }
        _ => None,
    }
}

fn normalize_rollout_response_item(item_type: &str, payload: &Map<String, Value>) -> Option<Value> {
    match item_type {
        "custom_tool_call" if payload.get("name").and_then(Value::as_str) == Some("exec") => Some(json!({
            "type":"assistant",
            "message":{
                "content":[{
                    "type":"tool_use",
                    "name":"Bash",
                    "input":{"command": payload.get("input")?.as_str()?},
                }]
            }
        })),
        "custom_tool_call_output" => Some(json!({
            "type":"user",
            "tool_name":"Bash",
            "tool_response":payload.get("output").cloned().unwrap_or(Value::Null),
        })),
        "message" => normalize_rollout_message(payload),
        _ => None,
    }
}

fn normalize_rollout_message(payload: &Map<String, Value>) -> Option<Value> {
    let role = payload.get("role")?.as_str()?;
    let content = payload.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter_map(|block| {
            let block_type = block.get("type").and_then(Value::as_str)?;
            if matches!(block_type, "output_text" | "input_text" | "text") {
                block.get("text").and_then(Value::as_str)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    match role {
        "assistant" => Some(assistant_text(&text)),
        "user" => Some(json!({"type":"user","text":text})),
        "developer" | "system" => Some(json!({"type":"system"})),
        _ => None,
    }
}

fn assistant_text(text: &str) -> Value {
    json!({
        "type":"assistant",
        "message":{"content":[{"type":"text","text":text}]}
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn session() -> CodexProgressSession {
        CodexProgressSession::new(None)
    }

    #[test]
    fn stdout_turn_and_command_execution_map_to_worker_events() {
        let mut session = session();
        assert_eq!(
            session
                .normalize_stdout(&json!({"type":"thread.started","thread_id":"thread-1"}))
                .unwrap(),
            WorkerEvent::SessionStart {
                session_id: "thread-1".into(),
                source: SessionStartSource::Startup,
            }
        );
        assert!(matches!(
            session.normalize_stdout(&json!({"type":"turn.started"})).unwrap(),
            WorkerEvent::UserPromptSubmit { session_id, .. } if session_id == "thread-1"
        ));
        assert_eq!(
            session
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
            session
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
            session
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
        let mut session = session();
        session
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"same-thread"}))
            .unwrap();
        let repeated = session
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
            session.normalize_stdout(&json!({"type":"turn.started"})).unwrap(),
            WorkerEvent::UserPromptSubmit { session_id, .. } if session_id == "same-thread"
        ));
    }

    #[test]
    fn persisted_identity_survives_restart_and_is_removed_with_run_home() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("run-home");
        fs::create_dir_all(&home).unwrap();

        let mut first_process = CodexProgressSession::new(Some(home.clone()));
        let first = first_process
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"restart-thread"}))
            .unwrap();
        assert!(matches!(
            first,
            WorkerEvent::SessionStart {
                source: SessionStartSource::Startup,
                ..
            }
        ));
        assert_eq!(
            fs::read_to_string(home.join(THREAD_ID_MARKER)).unwrap(),
            "restart-thread"
        );

        let mut after_restart = CodexProgressSession::new(Some(home.clone()));
        let resumed = after_restart
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"restart-thread"}))
            .unwrap();
        assert!(matches!(
            resumed,
            WorkerEvent::SessionStart {
                source: SessionStartSource::Resume,
                ..
            }
        ));

        fs::remove_dir_all(&home).unwrap();
        fs::create_dir_all(&home).unwrap();
        let mut after_cleanup = CodexProgressSession::new(Some(home));
        let fresh = after_cleanup
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"restart-thread"}))
            .unwrap();
        assert!(matches!(
            fresh,
            WorkerEvent::SessionStart {
                source: SessionStartSource::Startup,
                ..
            }
        ));
    }

    #[test]
    fn operational_error_item_is_non_terminal_notification() {
        let mut session = session();
        session
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"thread-warning"}))
            .unwrap();
        let event = session
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
    fn additive_variants_are_tolerated_without_session_state() {
        let mut session = session();
        for raw in [
            json!({"type":"thread.future","new_field":true}),
            json!({"type":"item.completed","item":{"id":"item_0","type":"extension","payload":{}}}),
            json!({"type":"item.started","item":{"id":"item_0","type":"dynamic_tool_call"}}),
            json!({"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"done"}}),
        ] {
            assert!(matches!(
                session.normalize_stdout(&raw),
                Err(NormalizeError::UnknownEvent(_))
            ));
        }
    }

    #[test]
    fn known_stdout_records_require_string_fields() {
        let mut session = session();
        session
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"malformed-thread"}))
            .unwrap();
        for raw in [
            json!({"type":"item.started","item":{"type":"command_execution","command":42}}),
            json!({
                "type":"item.completed",
                "item":{"type":"command_execution","command":"echo hi","aggregated_output":{"text":"hi"}}
            }),
            json!({"type":"item.completed","item":{"type":"error","message":["warning"]}}),
        ] {
            assert!(matches!(
                session.normalize_stdout(&raw),
                Err(NormalizeError::MissingField(_))
            ));
        }
    }

    #[test]
    fn stdout_turn_aborted_is_not_synthesized() {
        let mut session = session();
        assert!(matches!(
            session.normalize_stdout(&json!({
                "type":"turn_aborted",
                "turn_id":"turn-1",
                "reason":"interrupted"
            })),
            Err(NormalizeError::UnknownEvent(_))
        ));
    }

    #[test]
    fn rollout_records_become_canonical_renderable_records() {
        let aborted = normalize_rollout(json!({
            "type":"event_msg",
            "payload":{"type":"turn_aborted","turn_id":"turn-1","reason":"interrupted"}
        }));
        assert_eq!(aborted["type"], "assistant");
        assert_eq!(aborted["message"]["content"][0]["text"], "turn aborted: interrupted");

        let call = normalize_rollout(json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "call_id":"call-1",
                "name":"exec",
                "input":"echo rollout"
            }
        }));
        assert_eq!(call["type"], "assistant");
        assert_eq!(call["message"]["content"][0]["name"], "Bash");
        assert_eq!(
            call["message"]["content"][0]["input"],
            json!({"command":"echo rollout"})
        );

        let output = normalize_rollout(json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":"call-1",
                "output":[{"type":"input_text","text":"rollout\n"}]
            }
        }));
        assert_eq!(output["type"], "user");
        assert_eq!(output["tool_name"], "Bash");
        assert_eq!(
            output["tool_response"],
            json!([{"type":"input_text","text":"rollout\n"}])
        );
    }

    #[test]
    fn transcript_discovery_is_scoped_to_exact_run_home() {
        let tmp = TempDir::new().unwrap();
        let run_a = tmp.path().join("run-a");
        let run_b = tmp.path().join("run-b");
        let sessions_a = run_a.join("sessions/2026/07/26");
        let sessions_b = run_b.join("sessions/2026/07/26");
        fs::create_dir_all(&sessions_a).unwrap();
        fs::create_dir_all(&sessions_b).unwrap();
        let expected = sessions_a.join("rollout-2026-07-26T03-48-52-duplicate-thread.jsonl");
        fs::write(&expected, "{}\n").unwrap();
        fs::write(
            sessions_b.join("rollout-2026-07-26T03-48-53-duplicate-thread.jsonl"),
            "{}\n",
        )
        .unwrap();

        let mut session = CodexProgressSession::new(Some(run_a));
        session
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"duplicate-thread"}))
            .unwrap();
        assert_eq!(
            session.transcript_path(&json!({"type":"turn.started"})),
            Some(fs::canonicalize(expected).unwrap().to_string_lossy().into_owned())
        );
    }

    #[test]
    fn transcript_discovery_chooses_newest_duplicate_within_run() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("run-home");
        let day = home.join("sessions/2026/07/26");
        fs::create_dir_all(&day).unwrap();
        let older = day.join("rollout-2026-07-26T03-48-52-duplicate-thread.jsonl");
        let newer = day.join("rollout-2026-07-26T03-48-53-duplicate-thread.jsonl");
        fs::write(older, "{}\n").unwrap();
        fs::write(&newer, "{}\n").unwrap();

        assert_eq!(
            discover_rollout_path(&home, "duplicate-thread"),
            Some(fs::canonicalize(newer).unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn transcript_discovery_rejects_symlinked_sessions_and_candidates() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let outside_rollout = outside.join("rollout-2026-07-26T00-00-00-race-thread.jsonl");
        fs::write(&outside_rollout, "{}\n").unwrap();

        let linked_sessions_home = tmp.path().join("linked-sessions-home");
        fs::create_dir_all(&linked_sessions_home).unwrap();
        symlink(&outside, linked_sessions_home.join("sessions")).unwrap();
        assert!(discover_rollout_path(&linked_sessions_home, "race-thread").is_none());

        let linked_file_home = tmp.path().join("linked-file-home");
        let day = linked_file_home.join("sessions/2026/07/26");
        fs::create_dir_all(&day).unwrap();
        symlink(
            &outside_rollout,
            day.join("rollout-2026-07-26T00-00-00-race-thread.jsonl"),
        )
        .unwrap();
        assert!(discover_rollout_path(&linked_file_home, "race-thread").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn transcript_discovery_rejects_candidate_replaced_after_scan() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("run-home");
        let day = home.join("sessions/2026/07/26");
        fs::create_dir_all(&day).unwrap();
        let candidate = day.join("rollout-2026-07-26T00-00-00-race-thread.jsonl");
        fs::write(&candidate, "{}\n").unwrap();
        let outside = tmp.path().join("outside.jsonl");
        fs::write(&outside, "{}\n").unwrap();

        let found = discover_rollout_path_after_scan(&home, "race-thread", || {
            fs::remove_file(&candidate).unwrap();
            symlink(&outside, &candidate).unwrap();
        });
        assert!(found.is_none());
    }
}
