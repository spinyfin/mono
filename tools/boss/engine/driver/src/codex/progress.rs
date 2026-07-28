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

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use boss_engine_codex_rollout::{
    CanonicalRolloutToolCall, canonical_rollout_tool_call as shared_canonical_rollout_tool_call, extract_text_blocks,
    is_rollout_tool_output,
};
use boss_protocol::{NormalizeError, SessionStartSource, StopReason, WorkerEvent};
use serde_json::{Map, Value, json};

use crate::{ProgressIdentityStore, ProgressSessionNormalizer, TranscriptSessionNormalizer};

const MAX_TRACKED_ROLLOUT_CALLS: usize = 256;

#[derive(Clone)]
pub(super) struct RolloutToolCall {
    tool_name: String,
    tool_input: Value,
}

/// Thin adapter over [`boss_engine_codex_rollout::canonical_rollout_tool_call`].
/// Progress tracking requires a `call_id`; transcript rendering does not.
/// Shared pure reshape (exec → Bash, argv coerce) lives in the lower crate so
/// `transcript-markdown` can reuse it without depending on this driver module.
fn canonical_rollout_tool_call(item_type: &str, payload: &Map<String, Value>) -> Option<(String, RolloutToolCall)> {
    let CanonicalRolloutToolCall {
        call_id,
        tool_name,
        tool_input,
    } = shared_canonical_rollout_tool_call(item_type, payload)?;
    let call_id = call_id?;
    Some((call_id, RolloutToolCall { tool_name, tool_input }))
}

/// Mutable state owned by one stdout reader.
///
/// `current_thread_id` never lives on the registry's shared driver.
/// `codex_home` is the exact run home derived from the reader's run id;
/// transcript discovery cannot escape into a sibling run.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub(super) struct CodexProgressSession {
    current_thread_id: Option<String>,
    codex_home: Option<PathBuf>,
    homes_root: Option<PathBuf>,
    run_id: Option<String>,
    identity_store: Option<Arc<dyn ProgressIdentityStore>>,
    transcript_path_cache: Option<(String, String)>,
    turn_terminal: bool,
    terminal_message: Option<String>,
}

impl CodexProgressSession {
    pub(super) fn new(
        codex_home: Option<PathBuf>,
        homes_root: Option<PathBuf>,
        run_id: Option<String>,
        identity_store: Option<Arc<dyn ProgressIdentityStore>>,
    ) -> Self {
        Self {
            current_thread_id: None,
            codex_home,
            homes_root,
            run_id,
            identity_store,
            transcript_path_cache: None,
            turn_terminal: false,
            terminal_message: None,
        }
    }

    fn classify_thread_start(&mut self, thread_id: &str) -> SessionStartSource {
        // Keep only the current identity in memory. The durable marker below
        // restores that one lifecycle identity after an engine restart
        // without an unbounded process-global set of every thread ever seen.
        if self.current_thread_id.as_deref() == Some(thread_id) {
            return SessionStartSource::Resume;
        }

        let (Some(run_id), Some(identity_store)) = (self.run_id.as_deref(), self.identity_store.as_deref()) else {
            return SessionStartSource::Startup;
        };
        match identity_store.claim_progress_identity(run_id, thread_id) {
            Ok(true) => SessionStartSource::Resume,
            Ok(false) => SessionStartSource::Startup,
            Err(err) => {
                tracing::warn!(
                    run_id,
                    %err,
                    "codex stdout: could not persist engine-owned thread identity"
                );
                SessionStartSource::Startup
            }
        }
    }

    fn normalize_stdout(&mut self, raw: &Value) -> Result<WorkerEvent, NormalizeError> {
        self.normalize_stdout_events(raw)?
            .pop()
            .ok_or_else(|| NormalizeError::UnknownEvent("duplicate terminal Codex event".to_owned()))
    }

    fn normalize_stdout_events(&mut self, raw: &Value) -> Result<Vec<WorkerEvent>, NormalizeError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| NormalizeError::Malformed("expected Codex stdout JSON object".into()))?;
        let envelope = parse_stdout_envelope(obj)?;

        match envelope {
            StdoutEnvelope::ThreadStarted { thread_id } => {
                let source = self.classify_thread_start(thread_id);
                if self.current_thread_id.as_deref() != Some(thread_id) {
                    self.transcript_path_cache = None;
                }
                // A repeated thread.started from `exec resume` makes the same
                // thread current again without clearing any per-stream state.
                self.current_thread_id = Some(thread_id.to_owned());
                self.turn_terminal = false;
                self.terminal_message = None;
                // Codex stdout `thread.started` has no model field; SessionStart
                // model remains None and the live-worker reducer keeps the
                // launch default until a Claude-compatible hook supplies one.
                Ok(vec![WorkerEvent::SessionStart {
                    session_id: thread_id.to_owned(),
                    source,
                    model: None,
                }])
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
                    StdoutEnvelope::TurnStarted => {
                        self.turn_terminal = false;
                        self.terminal_message = None;
                        vec![WorkerEvent::UserPromptSubmit {
                            session_id,
                            prompt: String::new(),
                        }]
                    }
                    StdoutEnvelope::TurnCompleted => {
                        if self.turn_terminal {
                            tracing::debug!("codex stdout: suppressing a duplicate terminal turn.completed");
                            Vec::new()
                        } else {
                            self.turn_terminal = true;
                            vec![WorkerEvent::Stop {
                                session_id,
                                stop_hook_active: false,
                                stop_reason: StopReason::Completed,
                            }]
                        }
                    }
                    StdoutEnvelope::FatalError { message } => {
                        let duplicate_message = self.terminal_message.as_deref() == Some(message);
                        if !self.turn_terminal {
                            self.turn_terminal = true;
                            self.terminal_message = Some(message.to_owned());
                            vec![
                                WorkerEvent::Notification {
                                    session_id: session_id.clone(),
                                    message: message.to_owned(),
                                },
                                WorkerEvent::Stop {
                                    session_id,
                                    stop_hook_active: false,
                                    stop_reason: StopReason::Other,
                                },
                            ]
                        } else if duplicate_message {
                            tracing::debug!(message, "codex stdout: suppressing duplicate terminal error message");
                            Vec::new()
                        } else {
                            self.terminal_message = Some(message.to_owned());
                            vec![WorkerEvent::Notification {
                                session_id,
                                message: message.to_owned(),
                            }]
                        }
                    }
                    StdoutEnvelope::CommandStarted { command } => vec![WorkerEvent::PreToolUse {
                        session_id,
                        tool_name: "Bash".to_owned(),
                        tool_input: json!({ "command": command }),
                    }],
                    StdoutEnvelope::CommandCompleted { command, output } => vec![WorkerEvent::PostToolUse {
                        session_id,
                        tool_name: "Bash".to_owned(),
                        tool_input: json!({ "command": command }),
                        // Keep aggregated_output as a bare string. The shared
                        // PR-URL capture seam explicitly supports this shape.
                        tool_response: Value::String(output.to_owned()),
                    }],
                    StdoutEnvelope::OperationalWarning { message } => vec![WorkerEvent::Notification {
                        session_id,
                        message: message.to_owned(),
                    }],
                    StdoutEnvelope::ThreadStarted { .. } | StdoutEnvelope::Unknown { .. } => {
                        unreachable!("handled above")
                    }
                })
            }
        }
    }

    fn transcript_path(&mut self, raw: &Value) -> Option<String> {
        let thread_id = raw
            .get("thread_id")
            .and_then(Value::as_str)
            .or(self.current_thread_id.as_deref())?;
        if let Some((cached_thread_id, cached_path)) = self.transcript_path_cache.as_ref()
            && cached_thread_id == thread_id
        {
            return Some(cached_path.clone());
        }
        let codex_home = self.codex_home.as_deref()?;
        let homes_root = self.homes_root.as_deref()?;
        let path = discover_rollout_path(homes_root, codex_home, thread_id)?;
        let path = path.to_string_lossy().into_owned();
        self.transcript_path_cache = Some((thread_id.to_owned(), path.clone()));
        Some(path)
    }
}

impl ProgressSessionNormalizer for CodexProgressSession {
    fn normalize_progress_event(&mut self, raw: &Value) -> Result<WorkerEvent, NormalizeError> {
        self.normalize_stdout(raw)
    }

    fn normalize_progress_events(&mut self, raw: &Value) -> Result<Vec<WorkerEvent>, NormalizeError> {
        self.normalize_stdout_events(raw)
    }

    fn transcript_path_for_session(&mut self, raw: &Value) -> Option<String> {
        self.transcript_path(raw)
    }
}

/// Mutable state owned by one rollout-file reader.
///
/// Rollout records carry the thread id only on `session_meta`, and tool
/// outputs carry only a `call_id`. Keeping both correlations inside the
/// reader-owned session prevents concurrent Codex runs from sharing state.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub(super) struct CodexRolloutProgressSession {
    current_thread_id: Option<String>,
    run_id: Option<String>,
    identity_store: Option<Arc<dyn ProgressIdentityStore>>,
    transcript_path: Option<PathBuf>,
    calls: HashMap<String, RolloutToolCall>,
    call_order: VecDeque<String>,
    turn_terminal: bool,
}

impl CodexRolloutProgressSession {
    pub(super) fn new(
        run_id: Option<String>,
        identity_store: Option<Arc<dyn ProgressIdentityStore>>,
        transcript_path: Option<PathBuf>,
    ) -> Self {
        Self {
            current_thread_id: None,
            run_id,
            identity_store,
            transcript_path,
            calls: HashMap::new(),
            call_order: VecDeque::new(),
            turn_terminal: false,
        }
    }

    fn classify_thread_start(&self, thread_id: &str) -> SessionStartSource {
        let (Some(run_id), Some(identity_store)) = (self.run_id.as_deref(), self.identity_store.as_deref()) else {
            return SessionStartSource::Startup;
        };
        match identity_store.claim_progress_identity(run_id, thread_id) {
            Ok(true) => SessionStartSource::Resume,
            Ok(false) => SessionStartSource::Startup,
            Err(err) => {
                tracing::warn!(
                    run_id,
                    %err,
                    "codex rollout: could not persist engine-owned thread identity"
                );
                SessionStartSource::Startup
            }
        }
    }

    fn remember_call(&mut self, call_id: String, call: RolloutToolCall) {
        if let std::collections::hash_map::Entry::Occupied(mut existing) = self.calls.entry(call_id.clone()) {
            existing.insert(call);
            return;
        }
        while self.calls.len() >= MAX_TRACKED_ROLLOUT_CALLS {
            let Some(oldest) = self.call_order.pop_front() else {
                break;
            };
            self.calls.remove(&oldest);
        }
        self.calls.insert(call_id.clone(), call);
        self.call_order.push_back(call_id);
    }

    fn take_call(&mut self, call_id: &str) -> Option<RolloutToolCall> {
        let call = self.calls.remove(call_id)?;
        if let Some(index) = self.call_order.iter().position(|known| known == call_id) {
            self.call_order.remove(index);
        }
        Some(call)
    }

    fn session_id(&self) -> Result<String, NormalizeError> {
        self.current_thread_id
            .clone()
            .ok_or(NormalizeError::MissingField("session_meta.payload.id"))
    }

    fn normalize_events(&mut self, raw: &Value) -> Result<Vec<WorkerEvent>, NormalizeError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| NormalizeError::Malformed("expected Codex rollout JSON object".into()))?;
        let record_type = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or(NormalizeError::MissingField("type"))?;
        let payload = obj
            .get("payload")
            .and_then(Value::as_object)
            .ok_or(NormalizeError::MissingField("payload"))?;

        match record_type {
            "session_meta" => {
                let thread_id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or(NormalizeError::MissingField("payload.id"))?;
                let source = self.classify_thread_start(thread_id);
                self.current_thread_id = Some(thread_id.to_owned());
                self.turn_terminal = false;
                self.calls.clear();
                self.call_order.clear();
                Ok(vec![WorkerEvent::SessionStart {
                    session_id: thread_id.to_owned(),
                    source,
                    model: None,
                }])
            }
            "event_msg" => {
                let event_type = payload
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or(NormalizeError::MissingField("payload.type"))?;
                let session_id = self.session_id()?;
                match event_type {
                    "task_started" => {
                        self.turn_terminal = false;
                        Ok(vec![WorkerEvent::UserPromptSubmit {
                            session_id,
                            prompt: String::new(),
                        }])
                    }
                    "task_complete" if !self.turn_terminal => {
                        self.turn_terminal = true;
                        Ok(vec![WorkerEvent::Stop {
                            session_id,
                            stop_hook_active: false,
                            stop_reason: StopReason::Completed,
                        }])
                    }
                    "task_complete" => Err(NormalizeError::UnknownEvent(
                        "duplicate rollout task_complete".to_owned(),
                    )),
                    "turn_aborted" if !self.turn_terminal => {
                        self.turn_terminal = true;
                        let reason = payload.get("reason").and_then(Value::as_str).unwrap_or("interrupted");
                        Ok(vec![
                            WorkerEvent::Notification {
                                session_id: session_id.clone(),
                                message: format!("turn aborted: {reason}"),
                            },
                            WorkerEvent::Stop {
                                session_id,
                                stop_hook_active: false,
                                stop_reason: StopReason::Other,
                            },
                        ])
                    }
                    "turn_aborted" => Err(NormalizeError::UnknownEvent(
                        "duplicate rollout turn_aborted".to_owned(),
                    )),
                    _ => Err(NormalizeError::UnknownEvent(format!("event_msg/{event_type}"))),
                }
            }
            "response_item" => {
                let item_type = payload
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or(NormalizeError::MissingField("payload.type"))?;
                let session_id = self.session_id()?;
                if let Some((call_id, call)) = canonical_rollout_tool_call(item_type, payload) {
                    let event = WorkerEvent::PreToolUse {
                        session_id,
                        tool_name: call.tool_name.clone(),
                        tool_input: call.tool_input.clone(),
                    };
                    self.remember_call(call_id, call);
                    return Ok(vec![event]);
                }
                if is_rollout_tool_output(item_type) {
                    let call_id = payload
                        .get("call_id")
                        .and_then(Value::as_str)
                        .ok_or(NormalizeError::MissingField("payload.call_id"))?;
                    let call = self.take_call(call_id).ok_or_else(|| {
                        NormalizeError::UnknownEvent(format!("unmatched rollout tool output {call_id}"))
                    })?;
                    return Ok(vec![WorkerEvent::PostToolUse {
                        session_id,
                        tool_name: call.tool_name,
                        tool_input: call.tool_input,
                        // Preserve the rollout dialect's exact `payload.output`
                        // value. Codex's PR capture override flattens the
                        // observed string/array forms without pretending this
                        // is stdout's `aggregated_output`.
                        tool_response: payload.get("output").cloned().unwrap_or(Value::Null),
                    }]);
                }
                Err(NormalizeError::UnknownEvent(format!("response_item/{item_type}")))
            }
            _ => Err(NormalizeError::UnknownEvent(record_type.to_owned())),
        }
    }
}

impl ProgressSessionNormalizer for CodexRolloutProgressSession {
    fn normalize_progress_event(&mut self, raw: &Value) -> Result<WorkerEvent, NormalizeError> {
        self.normalize_events(raw)?
            .pop()
            .ok_or_else(|| NormalizeError::UnknownEvent("empty rollout event batch".to_owned()))
    }

    fn normalize_progress_events(&mut self, raw: &Value) -> Result<Vec<WorkerEvent>, NormalizeError> {
        self.normalize_events(raw)
    }

    fn transcript_path_for_session(&mut self, _raw: &Value) -> Option<String> {
        self.transcript_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
    }
}

/// Parsed stdout dialect. This is intentionally not reused for rollout lines.
enum StdoutEnvelope<'a> {
    ThreadStarted {
        thread_id: &'a str,
    },
    TurnStarted,
    TurnCompleted,
    FatalError {
        message: &'a str,
    },
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
        "turn.failed" => {
            let message = obj
                .get("error")
                .and_then(Value::as_object)
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .ok_or(NormalizeError::MissingField("error.message"))?;
            Ok(StdoutEnvelope::FatalError { message })
        }
        "error" => {
            let message = obj
                .get("message")
                .and_then(Value::as_str)
                .ok_or(NormalizeError::MissingField("message"))?;
            Ok(StdoutEnvelope::FatalError { message })
        }
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

/// Find a rollout only within this run's canonical `sessions` subtree.
///
/// Codex embeds a local timestamp before the thread id, so pure construction
/// is impossible. The run home must be a real direct child of the expected
/// canonical homes root; symlinked roots, homes, directories, and candidates
/// are rejected.
fn discover_rollout_path(homes_root: &Path, codex_home: &Path, thread_id: &str) -> Option<PathBuf> {
    discover_rollout_path_after_scan(homes_root, codex_home, thread_id, || {})
}

fn discover_rollout_path_after_scan(
    homes_root: &Path,
    codex_home: &Path,
    thread_id: &str,
    after_scan: impl FnOnce(),
) -> Option<PathBuf> {
    let roots = verify_transcript_roots(homes_root, codex_home)?;
    let canonical_sessions = roots.canonical_sessions.clone();

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

    // Re-check root/home/session identities after traversal. TranscriptTail's
    // contained open repeats these checks against the opened descriptor at
    // every poll, so replacement after this function returns is also refused.
    after_scan();
    let roots_now = verify_transcript_roots(homes_root, codex_home)?;
    let candidate_now = fs::canonicalize(&candidate).ok()?;
    if roots_now.canonical_homes != roots.canonical_homes
        || roots_now.canonical_home != roots.canonical_home
        || roots_now.canonical_sessions != canonical_sessions
        || candidate_now != candidate
        || !candidate_now.starts_with(&canonical_sessions)
        || fs::symlink_metadata(&candidate).ok()?.file_type().is_symlink()
    {
        return None;
    }
    Some(candidate)
}

struct VerifiedTranscriptRoots {
    canonical_homes: PathBuf,
    canonical_home: PathBuf,
    canonical_sessions: PathBuf,
}

fn verify_transcript_roots(homes_root: &Path, codex_home: &Path) -> Option<VerifiedTranscriptRoots> {
    if fs::symlink_metadata(homes_root).ok()?.file_type().is_symlink()
        || fs::symlink_metadata(codex_home).ok()?.file_type().is_symlink()
    {
        return None;
    }
    let canonical_homes = fs::canonicalize(homes_root).ok()?;
    let canonical_home = fs::canonicalize(codex_home).ok()?;
    if canonical_home == canonical_homes || !canonical_home.starts_with(&canonical_homes) {
        return None;
    }
    let sessions_path = codex_home.join("sessions");
    if fs::symlink_metadata(&sessions_path).ok()?.file_type().is_symlink() {
        return None;
    }
    let canonical_sessions = fs::canonicalize(&sessions_path).ok()?;
    if canonical_sessions == canonical_home || !canonical_sessions.starts_with(&canonical_home) {
        return None;
    }
    Some(VerifiedTranscriptRoots {
        canonical_homes,
        canonical_home,
        canonical_sessions,
    })
}

pub(super) fn verified_sessions_root(homes_root: &Path, codex_home: &Path) -> Option<PathBuf> {
    verify_transcript_roots(homes_root, codex_home).map(|roots| roots.canonical_sessions)
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
/// Direct callers get an isolated one-record session. The live-status loop
/// uses [`CodexTranscriptSession`] for multi-record tool correlation.
pub(super) fn normalize_rollout(raw: Value) -> Value {
    CodexTranscriptSession::default().normalize(raw)
}

#[derive(Default)]
pub(super) struct CodexTranscriptSession {
    tool_calls: HashMap<String, RolloutToolCall>,
    exec_order: VecDeque<String>,
}

impl CodexTranscriptSession {
    fn normalize(&mut self, raw: Value) -> Value {
        let parsed = match raw.as_object().and_then(parse_rollout_envelope) {
            Some(parsed) => parsed,
            None => {
                tracing::debug!("codex rollout: ignoring malformed/non-object transcript record");
                return raw;
            }
        };

        match parsed {
            RolloutEnvelope::SessionMeta => json!({"type":"system"}),
            RolloutEnvelope::EventMessage { event_type, payload } => {
                normalize_rollout_event_message(&event_type, &payload).unwrap_or_else(|| {
                    tracing::debug!(event_type, "codex rollout: ignoring additive event_msg variant");
                    raw
                })
            }
            RolloutEnvelope::ResponseItem { item_type, payload } => self
                .normalize_rollout_response_item(&item_type, &payload)
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

    fn remember_exec(&mut self, call_id: &str, call: RolloutToolCall) {
        if let std::collections::hash_map::Entry::Occupied(mut existing) = self.tool_calls.entry(call_id.to_owned()) {
            existing.insert(call);
            return;
        }
        while self.tool_calls.len() >= MAX_TRACKED_ROLLOUT_CALLS {
            let Some(oldest) = self.exec_order.pop_front() else {
                break;
            };
            self.tool_calls.remove(&oldest);
        }
        self.tool_calls.insert(call_id.to_owned(), call);
        self.exec_order.push_back(call_id.to_owned());
    }

    fn take_exec(&mut self, call_id: &str) -> Option<RolloutToolCall> {
        let call = self.tool_calls.remove(call_id)?;
        if let Some(index) = self.exec_order.iter().position(|known| known == call_id) {
            self.exec_order.remove(index);
        }
        Some(call)
    }

    fn normalize_rollout_response_item(&mut self, item_type: &str, payload: &Map<String, Value>) -> Option<Value> {
        if let Some((call_id, call)) = canonical_rollout_tool_call(item_type, payload) {
            let normalized = json!({
                "type":"assistant",
                "content":[{
                    "type":"tool_use",
                    "name":call.tool_name,
                    "input":call.tool_input,
                }]
            });
            self.remember_exec(&call_id, call);
            return Some(normalized);
        }
        if is_rollout_tool_output(item_type) {
            let call_id = payload.get("call_id")?.as_str()?;
            let Some(call) = self.take_exec(call_id) else {
                tracing::debug!(call_id, "codex rollout: omitting unmatched tool output body");
                return Some(json!({"type":"system"}));
            };
            return Some(json!({
                "type":"user",
                "tool_name":call.tool_name,
                "tool_input":call.tool_input,
                "tool_response":payload.get("output").cloned().unwrap_or(Value::Null),
            }));
        }
        (item_type == "message")
            .then(|| normalize_rollout_message(payload))
            .flatten()
    }
}

impl TranscriptSessionNormalizer for CodexTranscriptSession {
    fn normalize_transcript_entry(&mut self, raw: Value) -> Value {
        self.normalize(raw)
    }
}

fn normalize_rollout_event_message(event_type: &str, payload: &Map<String, Value>) -> Option<Value> {
    match event_type {
        "agent_message" => Some(assistant_text(payload.get("message")?.as_str()?)),
        "user_message" => Some(json!({
            "type":"user",
            "text": payload.get("message")?.as_str()?,
        })),
        // Lifecycle fillers are `system`, not assistant text. Emitting them as
        // AssistantText made every Codex Stop-boundary read that landed on a
        // partial rollout (session_meta + task_started, final agent_message
        // not yet flushed) treat the synthetic "turn started" as worker prose,
        // short-circuit the flush-race retry, and permanently miss markers.
        "turn_aborted" => {
            let reason = payload.get("reason").and_then(Value::as_str).unwrap_or("interrupted");
            Some(lifecycle_system("turn_aborted", &format!("turn aborted: {reason}")))
        }
        "task_started" => Some(lifecycle_system("task_started", "turn started")),
        "task_complete" => {
            // Real final prose stays AssistantText so marker scans can see it.
            // The bare "turn completed" filler is lifecycle-only.
            match payload.get("last_agent_message").and_then(Value::as_str) {
                Some(message) => Some(assistant_text(message)),
                None => Some(lifecycle_system("task_complete", "turn completed")),
            }
        }
        _ => None,
    }
}

/// Codex turn-boundary filler, tagged as system so it is not worker prose.
///
/// Marker scans and the Stop-boundary flush-race retry gate on
/// `AssistantText`. Lifecycle placeholders must not count: a synthetic
/// "turn started" on a partial rollout used to make `all_text` non-empty and
/// disable the retry, permanently dropping a late-flushed `[blocked]` marker.
fn lifecycle_system(subtype: &str, message: &str) -> Value {
    json!({
        "type": "system",
        "subtype": subtype,
        "message": message,
    })
}

fn normalize_rollout_message(payload: &Map<String, Value>) -> Option<Value> {
    let role = payload.get("role")?.as_str()?;
    let content = payload.get("content")?.as_array()?;
    // Text-block extraction is shared with transcript-markdown via
    // `boss_engine_codex_rollout::extract_text_blocks`.
    let text = extract_text_blocks(content);
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
        "content":[{"type":"text","text":text}]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    fn session() -> CodexProgressSession {
        CodexProgressSession::new(None, None, None, None)
    }

    #[derive(Default)]
    struct MemoryIdentityStore(Mutex<HashMap<String, String>>);

    impl ProgressIdentityStore for MemoryIdentityStore {
        fn claim_progress_identity(&self, run_id: &str, session_id: &str) -> Result<bool, String> {
            let mut identities = self.0.lock().map_err(|_| "identity lock poisoned".to_owned())?;
            let resumed = identities.get(run_id).map(String::as_str) == Some(session_id);
            identities.insert(run_id.to_owned(), session_id.to_owned());
            Ok(resumed)
        }
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
                model: None,
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
                model: None,
            }
        );
        assert!(matches!(
            session.normalize_stdout(&json!({"type":"turn.started"})).unwrap(),
            WorkerEvent::UserPromptSubmit { session_id, .. } if session_id == "same-thread"
        ));
    }

    #[test]
    fn engine_owned_identity_survives_progress_reader_restart() {
        let store = Arc::new(MemoryIdentityStore::default());
        let mut first_process = CodexProgressSession::new(None, None, Some("run-restart".into()), Some(store.clone()));
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
        let mut after_restart = CodexProgressSession::new(None, None, Some("run-restart".into()), Some(store));
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

        let mut after_cleanup = CodexProgressSession::new(
            None,
            None,
            Some("run-restart".into()),
            Some(Arc::new(MemoryIdentityStore::default())),
        );
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
    fn fatal_stdout_envelopes_preserve_message_and_end_the_turn() {
        for (raw, expected_message) in [
            (
                json!({
                    "type":"turn.failed",
                    "error":{"message":"upstream quota exhausted"}
                }),
                "upstream quota exhausted",
            ),
            (
                json!({
                    "type":"error",
                    "message":"unrecoverable response stream failure"
                }),
                "unrecoverable response stream failure",
            ),
        ] {
            let mut session = session();
            session
                .normalize_stdout(&json!({"type":"thread.started","thread_id":"fatal-thread"}))
                .unwrap();
            session.normalize_stdout(&json!({"type":"turn.started"})).unwrap();

            let events = session.normalize_stdout_events(&raw).unwrap();
            assert_eq!(
                events,
                vec![
                    WorkerEvent::Notification {
                        session_id: "fatal-thread".into(),
                        message: expected_message.into(),
                    },
                    WorkerEvent::Stop {
                        session_id: "fatal-thread".into(),
                        stop_hook_active: false,
                        stop_reason: StopReason::Other,
                    },
                ]
            );
        }
    }

    #[test]
    fn top_level_error_and_turn_failed_emit_only_one_terminal_boundary() {
        let mut session = session();
        session
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"fatal-thread"}))
            .unwrap();
        session.normalize_stdout(&json!({"type":"turn.started"})).unwrap();

        let first = session
            .normalize_stdout_events(&json!({"type":"error","message":"same upstream failure"}))
            .unwrap();
        let duplicate = session
            .normalize_stdout_events(&json!({
                "type":"turn.failed",
                "error":{"message":"same upstream failure"}
            }))
            .unwrap();

        assert_eq!(
            first
                .iter()
                .filter(|event| matches!(event, WorkerEvent::Stop { .. }))
                .count(),
            1
        );
        assert!(
            duplicate.is_empty(),
            "the upstream terminal summary must not emit a second Stop"
        );
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
            json!({"type":"turn.failed","error":{"message":["fatal"]}}),
            json!({"type":"error","message":{"text":"fatal"}}),
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
        // Lifecycle fillers are system (not assistant) so they don't count as
        // worker prose for marker scans / flush-race retries.
        assert_eq!(aborted["type"], "system");
        assert_eq!(aborted["subtype"], "turn_aborted");
        assert_eq!(aborted["message"], "turn aborted: interrupted");

        let started = normalize_rollout(json!({
            "type":"event_msg",
            "payload":{"type":"task_started","turn_id":"turn-1"}
        }));
        assert_eq!(started["type"], "system");
        assert_eq!(started["subtype"], "task_started");
        assert_eq!(started["message"], "turn started");

        let bare_complete = normalize_rollout(json!({
            "type":"event_msg",
            "payload":{"type":"task_complete","turn_id":"turn-1"}
        }));
        assert_eq!(bare_complete["type"], "system");
        assert_eq!(bare_complete["message"], "turn completed");

        let complete_with_prose = normalize_rollout(json!({
            "type":"event_msg",
            "payload":{
                "type":"task_complete",
                "turn_id":"turn-1",
                "last_agent_message":"[blocked] reason=\"needs a decision\""
            }
        }));
        assert_eq!(complete_with_prose["type"], "assistant");
        assert_eq!(
            complete_with_prose["content"][0]["text"],
            "[blocked] reason=\"needs a decision\""
        );

        let mut session = CodexTranscriptSession::default();
        let call = session.normalize(json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "call_id":"call-1",
                "name":"exec",
                "input":"echo rollout"
            }
        }));
        assert_eq!(call["type"], "assistant");
        assert_eq!(call["content"][0]["name"], "Bash");
        assert_eq!(call["content"][0]["input"], json!({"command":"echo rollout"}));

        let output = session.normalize(json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":"call-1",
                "output":[{"type":"input_text","text":"rollout\n"}]
            }
        }));
        assert_eq!(output["type"], "user");
        assert_eq!(output["tool_name"], "Bash");
        assert_eq!(output["tool_input"], json!({"command":"echo rollout"}));
        assert_eq!(
            output["tool_response"],
            json!([{"type":"input_text","text":"rollout\n"}])
        );
    }

    #[test]
    fn real_rollout_dialect_maps_directly_to_ordered_worker_milestones() {
        let mut session = CodexRolloutProgressSession::new(
            Some("run-real".into()),
            None,
            Some(PathBuf::from("/tmp/rollout-real.jsonl")),
        );
        let records = [
            json!({
                "type":"session_meta",
                "payload":{"id":"thread-real","cwd":"/tmp/workspace"}
            }),
            json!({
                "type":"event_msg",
                "payload":{"type":"task_started","turn_id":"turn-real"}
            }),
            json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "name":"exec_command",
                    "call_id":"call-real",
                    "arguments":r#"{"cmd":"gh pr create --title test"}"#
                }
            }),
            json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call_output",
                    "call_id":"call-real",
                    "output":"https://github.com/example/repo/pull/7\n"
                }
            }),
            json!({
                "type":"event_msg",
                "payload":{
                    "type":"task_complete",
                    "turn_id":"turn-real",
                    "last_agent_message":"done"
                }
            }),
        ];
        let events = records
            .iter()
            .flat_map(|record| session.normalize_progress_events(record).unwrap())
            .collect::<Vec<_>>();

        assert!(matches!(
            &events[0],
            WorkerEvent::SessionStart { session_id, .. } if session_id == "thread-real"
        ));
        assert!(matches!(
            &events[1],
            WorkerEvent::UserPromptSubmit { session_id, .. } if session_id == "thread-real"
        ));
        assert_eq!(
            events[2],
            WorkerEvent::PreToolUse {
                session_id: "thread-real".into(),
                tool_name: "Bash".into(),
                tool_input: json!({"command":"gh pr create --title test"}),
            }
        );
        assert_eq!(
            events[3],
            WorkerEvent::PostToolUse {
                session_id: "thread-real".into(),
                tool_name: "Bash".into(),
                tool_input: json!({"command":"gh pr create --title test"}),
                tool_response: json!("https://github.com/example/repo/pull/7\n"),
            }
        );
        assert_eq!(
            events[4],
            WorkerEvent::Stop {
                session_id: "thread-real".into(),
                stop_hook_active: false,
                stop_reason: StopReason::Completed,
            }
        );
        assert_eq!(
            session.transcript_path_for_session(&records[4]).as_deref(),
            Some("/tmp/rollout-real.jsonl")
        );
    }

    #[test]
    fn rollout_custom_tool_variant_and_abort_preserve_ordered_fanout() {
        let mut session = CodexRolloutProgressSession::new(None, None, None);
        session
            .normalize_progress_events(&json!({
                "type":"session_meta",
                "payload":{"id":"thread-custom","cwd":"/tmp/workspace"}
            }))
            .unwrap();
        session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call",
                    "name":"exec",
                    "call_id":"call-custom",
                    "input":"printf custom"
                }
            }))
            .unwrap();
        let post = session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call_output",
                    "call_id":"call-custom",
                    "output":[{"type":"input_text","text":"custom\n"}]
                }
            }))
            .unwrap();
        assert!(matches!(
            &post[0],
            WorkerEvent::PostToolUse {
                tool_name,
                tool_response,
                ..
            } if tool_name == "Bash" && tool_response.is_array()
        ));

        let terminal = session
            .normalize_progress_events(&json!({
                "type":"event_msg",
                "payload":{"type":"turn_aborted","reason":"interrupted"}
            }))
            .unwrap();
        assert!(matches!(&terminal[0], WorkerEvent::Notification { .. }));
        assert!(matches!(
            &terminal[1],
            WorkerEvent::Stop {
                stop_reason: StopReason::Other,
                ..
            }
        ));
        assert!(matches!(
            session.normalize_progress_events(&json!({
                "type":"event_msg",
                "payload":{"type":"task_complete"}
            })),
            Err(NormalizeError::UnknownEvent(_))
        ));
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

        let mut session = CodexProgressSession::new(Some(run_a), Some(tmp.path().to_path_buf()), None, None);
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
            discover_rollout_path(tmp.path(), &home, "duplicate-thread"),
            Some(fs::canonicalize(newer).unwrap())
        );
    }

    #[test]
    fn successful_transcript_discovery_is_cached_and_thread_change_invalidates() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("run-home");
        let day = home.join("sessions/2026/07/26");
        fs::create_dir_all(&day).unwrap();
        let transcript = day.join("rollout-2026-07-26T03-48-52-thread-a.jsonl");
        fs::write(&transcript, "{}\n").unwrap();
        let expected = fs::canonicalize(&transcript).unwrap().to_string_lossy().into_owned();
        let mut session = CodexProgressSession::new(Some(home), Some(tmp.path().to_path_buf()), None, None);
        session
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"thread-a"}))
            .unwrap();
        assert_eq!(
            session.transcript_path(&json!({"type":"turn.started"})),
            Some(expected.clone())
        );

        fs::remove_file(transcript).unwrap();
        assert_eq!(
            session.transcript_path(&json!({"type":"turn.completed"})),
            Some(expected),
            "successful discovery must be reused instead of rescanning every envelope"
        );

        session
            .normalize_stdout(&json!({"type":"thread.started","thread_id":"thread-b"}))
            .unwrap();
        assert!(
            session.transcript_path(&json!({"type":"turn.started"})).is_none(),
            "a changed thread id must invalidate the prior transcript cache"
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
        assert!(discover_rollout_path(tmp.path(), &linked_sessions_home, "race-thread").is_none());

        let linked_file_home = tmp.path().join("linked-file-home");
        let day = linked_file_home.join("sessions/2026/07/26");
        fs::create_dir_all(&day).unwrap();
        symlink(
            &outside_rollout,
            day.join("rollout-2026-07-26T00-00-00-race-thread.jsonl"),
        )
        .unwrap();
        assert!(discover_rollout_path(tmp.path(), &linked_file_home, "race-thread").is_none());

        let real_home = tmp.path().join("real-run-home");
        fs::create_dir_all(real_home.join("sessions")).unwrap();
        let linked_home = tmp.path().join("linked-run-home");
        symlink(&real_home, &linked_home).unwrap();
        assert!(
            discover_rollout_path(tmp.path(), &linked_home, "race-thread").is_none(),
            "the run home itself must be a real directory under the expected homes root"
        );
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

        let found = discover_rollout_path_after_scan(tmp.path(), &home, "race-thread", || {
            fs::remove_file(&candidate).unwrap();
            symlink(&outside, &candidate).unwrap();
        });
        assert!(found.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn agent_controlled_legacy_marker_links_are_never_opened() {
        use std::os::unix::fs::symlink;

        for link_kind in ["symlink", "hardlink"] {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("run-home");
            fs::create_dir_all(&home).unwrap();
            let victim = tmp.path().join("victim");
            fs::write(&victim, format!("keep-{link_kind}")).unwrap();
            let marker = home.join(".boss-thread-id");
            match link_kind {
                "symlink" => symlink(&victim, &marker).unwrap(),
                "hardlink" => fs::hard_link(&victim, &marker).unwrap(),
                _ => unreachable!(),
            }

            let store = Arc::new(MemoryIdentityStore::default());
            let mut session = CodexProgressSession::new(
                Some(home),
                Some(tmp.path().to_path_buf()),
                Some(format!("run-{link_kind}")),
                Some(store),
            );
            session
                .normalize_stdout(&json!({"type":"thread.started","thread_id":"hostile-thread"}))
                .unwrap();

            assert_eq!(
                fs::read_to_string(&victim).unwrap(),
                format!("keep-{link_kind}"),
                "engine-owned identity persistence must not touch a legacy marker link"
            );
        }
    }
}
