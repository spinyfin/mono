//! Codex progress and transcript normalisation.
//!
//! Codex's progress ingress is the rollout-file dialect only: pane-hosted
//! Codex stdout belongs to Ghostty's pty master, which the engine cannot
//! read, so the engine tails the run-private rollout JSONL file instead. One
//! [`CodexRolloutProgressSession`] owns correlation for one reader over
//! `session_meta`, `event_msg`, and `response_item` records, which are
//! reshaped into the canonical user/assistant/tool records consumed by
//! live-status rendering.
//!
//! An engine-spawned `codex exec --json` stdout-JSONL dialect (`thread.*`,
//! `turn.*`, `item.*` envelopes) previously existed alongside this one, but
//! the engine-owns-the-pipe topology it required was never pursued — see
//! `tools/boss/docs/designs/codex-as-a-first-class-agent-driver.md` — so it
//! was removed rather than kept unreachable.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use boss_engine_codex_rollout::{
    CanonicalRolloutToolCall, canonical_rollout_tool_call as shared_canonical_rollout_tool_call, extract_text_blocks,
    is_rollout_tool_output,
};
use boss_protocol::{NormalizeError, SessionStartSource, StopReason, WorkerEvent};
use serde_json::{Map, Value, json};

use super::guard_chain::{self, ArmedChainStatus};
use super::guard_trace;
use super::rollout_calls::{
    CallDisposition, CorrelationError, MAX_TRACKED_TRANSCRIPT_TOOL_CALLS, OutputDisposition, RolloutCallSnapshot,
    RolloutCallTracker, RolloutToolCall, abandoned_call_display, cell_aware_tool_output,
};
use crate::{ProgressIdentityStore, ProgressSessionNormalizer, TranscriptSessionNormalizer};

/// Thin adapter over [`boss_engine_codex_rollout::canonical_rollout_tool_call`].
/// Progress tracking requires a `call_id`; transcript rendering does not.
/// Shared pure reshape (exec → Bash, argv coerce) lives in the lower crate so
/// `transcript-markdown` can reuse it without depending on this driver module.
fn canonical_rollout_tool_call(item_type: &str, payload: &Map<String, Value>) -> Option<(String, RolloutToolCall)> {
    let CanonicalRolloutToolCall {
        call_id,
        tool_name,
        tool_input,
        issues_command,
    } = shared_canonical_rollout_tool_call(item_type, payload)?;
    let call_id = call_id?;
    Some((
        call_id,
        RolloutToolCall {
            tool_name,
            tool_input,
            issues_command,
            open_poll: None,
        },
    ))
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
    calls: RolloutCallTracker,
    turn_terminal: bool,
    /// The run's Boss-owned `CODEX_HOME`, when the reader knows which run it
    /// belongs to. Absent in fixtures and for the stateless path. Both the
    /// guard decision log and the arming attestation are resolved from it.
    codex_home: Option<PathBuf>,
    /// Trace lines already reported, so a later turn does not re-report them.
    guard_trace_line: usize,
    /// Tool calls observed since the last turn boundary. Compared against the
    /// guard records read for that turn.
    observed_tool_calls: usize,
    /// Whether any guard record has ever been read for this run.
    ///
    /// Run-scoped, not per-turn, because of a structural false positive:
    /// `observed_tool_calls` counts every rollout `custom_tool_call`, which on
    /// a code-mode model is the JavaScript cell itself, and a cell that invokes
    /// no inner tool fires no `PreToolUse` at all (verified in the
    /// investigation doc, section 3). So an ordinary turn whose cell only
    /// computes, prints, or touches `store`/`load`/`notify` used to log
    /// "command guardrails were not enforced" at `error`.
    ///
    /// What this flag must **not** be read as is "the guards are still armed".
    /// Liveness is re-checked against disk at every turn boundary by
    /// [`guard_chain::armed_chain_status`]. This flag's only job is to keep a
    /// turn whose cell invoked no inner tool from alarming.
    #[builder(default)]
    guard_records_seen: bool,
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
            calls: RolloutCallTracker::default(),
            turn_terminal: false,
            codex_home: None,
            guard_trace_line: 0,
            observed_tool_calls: 0,
            guard_records_seen: false,
        }
    }

    /// Point this reader at the run's Boss-owned `CODEX_HOME`.
    ///
    /// Separate from [`Self::new`] so the many fixtures constructing a bare
    /// session stay untouched: a reader with no `CODEX_HOME` reports no guard
    /// activity and makes no claim about arming, which is the correct answer
    /// for a synthetic rollout.
    pub(super) fn with_codex_home(mut self, codex_home: Option<PathBuf>) -> Self {
        self.codex_home = codex_home;
        self
    }

    /// Guard-activity notifications for the turn that is ending.
    ///
    /// Three conditions, all engine-visible, and they compose — a turn can
    /// report a broken chain *and* what the guards still armed decided:
    ///
    /// - the armed chain is no longer intact on disk → one
    ///   [`super::GUARDS_SILENT_MARKER`] notification, **every** turn it stays
    ///   broken and regardless of whether this turn ran a tool call. Arming is
    ///   a one-time act and a session outlives it, so "armed" is
    ///   re-established rather than remembered. See [`guard_chain`];
    /// - records exist → one [`super::GUARD_TRACE_MARKER`] notification
    ///   summarising them, so "did the guard fire, and what did it decide?" is
    ///   answerable from the trace for every Codex execution. Emitted whether
    ///   or not the chain check passed: verification stops at the first bad
    ///   entry, so the guards behind it keep running and recording;
    /// - tool calls ran and **no guard record has been read for this run at
    ///   all** → one [`super::GUARDS_SILENT_MARKER`] notification, the signal
    ///   that Codex skipped its hooks from the very start (untrusted trust
    ///   record, unexecutable handler) and every guardrail was inert while the
    ///   run looked healthy. Suppressed when the chain check already reported
    ///   a break, which carries the same marker with a more specific detail.
    ///
    /// The third condition is run-scoped rather than per-turn: see
    /// [`Self::guard_records_seen`] for why a per-turn comparison fires on
    /// ordinary code-mode cells that invoke no tool. The first is what keeps
    /// that scoping honest over a run that can span hours — it is a fresh
    /// check, not a widened window, so a chain that breaks after the latch is
    /// set is still reported.
    ///
    /// Called immediately before a `Stop`, mirroring
    /// [`Self::drain_abandoned_command_notifications`].
    fn drain_guard_trace_notifications(&mut self, session_id: &str) -> Vec<WorkerEvent> {
        let observed = std::mem::take(&mut self.observed_tool_calls);
        let Some(codex_home) = self.codex_home.clone() else {
            return Vec::new();
        };

        let mut events = Vec::new();

        // Ask disk, not history, whether the guards are still reachable:
        // records from earlier turns do not make the current turn guarded.
        let chain_broken = match guard_chain::armed_chain_status(Some(&codex_home)) {
            ArmedChainStatus::Broken(detail) => {
                events.push(WorkerEvent::Notification {
                    session_id: session_id.to_owned(),
                    message: super::guard_chain_broken_notification(&detail),
                });
                true
            }
            ArmedChainStatus::Intact | ArmedChainStatus::Unknown => false,
        };

        // Drain the trace either way. A broken chain is broken at its first bad
        // entry, so a run that lost one wrapper of five still has four guards
        // recording — and a `block` they issue while the chain is degraded is
        // exactly what an operator needs to see.
        let read = guard_trace::read_records_from(&guard_trace::guard_trace_path(&codex_home), self.guard_trace_line);
        self.guard_trace_line = read.next_line;
        if read.records.is_empty() && read.unparseable_lines == 0 {
            // With the chain intact, a turn with no new record is a turn whose
            // cells called no guarded tool — not a disarmed guardrail. With the
            // chain broken it is already reported above, and a second
            // notification under the same marker adds nothing.
            if !chain_broken && observed > 0 && !self.guard_records_seen {
                events.push(WorkerEvent::Notification {
                    session_id: session_id.to_owned(),
                    message: super::guards_silent_notification(observed),
                });
            }
            return events;
        }
        self.guard_records_seen = true;
        let summary = guard_trace::summarize(&read);
        events.push(WorkerEvent::Notification {
            session_id: session_id.to_owned(),
            message: super::guard_trace_notification(&summary),
        });
        events
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

    /// Abandoned-command detection (probe 6, exit-code investigation): drain
    /// every rollout tool call that started (`function_call` /
    /// `custom_tool_call`) but never received a terminal output into a
    /// [`WorkerEvent::Notification`] carrying
    /// [`super::UNOBSERVED_COMMAND_MARKER`], oldest-first. A call that issued
    /// a shell command is flagged by that command. A `write_stdin` poll with
    /// no tracked origin that is still open is flagged by session id (or as
    /// an unresolved poll when the session expression was dynamic).
    /// Compute-only cells and non-shell tools (`apply_patch`, file reads,
    /// MCP tools, `tools.view_image`) are dropped from tracking without a
    /// notification; they never gate a `no-changes-needed` claim. Called
    /// immediately before a `Stop` is emitted so a later, unrelated turn
    /// never re-flags the same call.
    ///
    /// "Terminal" is the cell-aware sense, and it is about the command, not
    /// the cell. Two shapes leave a call open here, and the model stopping
    /// without resolving either is what gets flagged:
    ///
    /// - a cell that yielded (`Script running with cell ID N`) and was never
    ///   polled at all;
    /// - a cell that yielded, was polled, and came back `Script completed`
    ///   with a chunk carrying no exit code — the command was still running
    ///   and the model stopped there. That is the shape probe 6 reproduced,
    ///   and treating its `Script completed` as terminal is what previously
    ///   made the reported failure invisible.
    fn drain_abandoned_command_notifications(&mut self, session_id: &str) -> Vec<WorkerEvent> {
        if !self.calls.has_open_calls() {
            return Vec::new();
        }
        self.calls
            .drain_open_calls()
            .into_iter()
            .filter_map(|call| abandoned_call_display(&call))
            .map(|display| WorkerEvent::Notification {
                session_id: session_id.to_owned(),
                message: super::unobserved_command_notification(&display),
            })
            .collect()
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
                self.calls.reset();
                self.observed_tool_calls = 0;
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
                        // A codex `task_complete` carrying a non-null `error`
                        // is a fatal provider/API failure, not a clean turn
                        // end — the process is about to exit on it. Emitting
                        // `StopReason::Completed` here is exactly the bug
                        // that let a dead worker's `task_complete` read as a
                        // normal idle turn: the engine has no other channel
                        // that tells it this run's driver hit an
                        // unrecoverable error. `StopReason::Other` is
                        // reserved for that signal.
                        match rollout_task_complete_error_message(payload) {
                            Some(message) => {
                                let mut events = self.drain_abandoned_command_notifications(&session_id);
                                events.extend(self.drain_guard_trace_notifications(&session_id));
                                events.push(WorkerEvent::Notification {
                                    session_id: session_id.clone(),
                                    message: message.clone(),
                                });
                                events.push(WorkerEvent::Stop {
                                    session_id,
                                    stop_hook_active: false,
                                    stop_reason: StopReason::Other,
                                });
                                Ok(events)
                            }
                            None => {
                                let mut events = self.drain_abandoned_command_notifications(&session_id);
                                events.extend(self.drain_guard_trace_notifications(&session_id));
                                events.push(WorkerEvent::Stop {
                                    session_id,
                                    stop_hook_active: false,
                                    stop_reason: StopReason::Completed,
                                });
                                Ok(events)
                            }
                        }
                    }
                    "task_complete" => Err(NormalizeError::UnknownEvent(
                        "duplicate rollout task_complete".to_owned(),
                    )),
                    "turn_aborted" if !self.turn_terminal => {
                        self.turn_terminal = true;
                        let reason = payload.get("reason").and_then(Value::as_str).unwrap_or("interrupted");
                        let mut events = self.drain_abandoned_command_notifications(&session_id);
                        events.extend(self.drain_guard_trace_notifications(&session_id));
                        events.push(WorkerEvent::Notification {
                            session_id: session_id.clone(),
                            message: format!("turn aborted: {reason}"),
                        });
                        events.push(WorkerEvent::Stop {
                            session_id,
                            stop_hook_active: false,
                            // Distinct from the fatal-error `Other` above:
                            // an abort is Codex acting on an interruption
                            // (its own default reason string is literally
                            // "interrupted"), not a provider/API failure.
                            stop_reason: StopReason::Interrupted,
                        });
                        Ok(events)
                    }
                    "turn_aborted" => Err(NormalizeError::UnknownEvent(
                        "duplicate rollout turn_aborted".to_owned(),
                    )),
                    // Benign bookkeeping envelope: per-turn token accounting,
                    // no progress or lifecycle signal to report.
                    "token_count" => Ok(Vec::new()),
                    "agent_message" | "agent_reasoning" => {
                        let has_output = payload
                            .get("message")
                            .or_else(|| payload.get("text"))
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.is_empty());
                        Ok(if has_output {
                            vec![WorkerEvent::ModelProgress { session_id }]
                        } else {
                            Vec::new()
                        })
                    }
                    _ => Err(NormalizeError::UnknownEvent(format!("event_msg/{event_type}"))),
                }
            }
            "response_item" => {
                let item_type = payload
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or(NormalizeError::MissingField("payload.type"))?;
                let session_id = self.session_id()?;
                // Current rollouts carry model output as response items;
                // older versions also emitted the event_msg forms above.
                let model_message = item_type == "message"
                    && payload.get("role").and_then(Value::as_str) == Some("assistant")
                    && payload.get("content").and_then(Value::as_array).is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            block
                                .get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|text| !text.is_empty())
                        })
                    });
                let model_reasoning = item_type == "reasoning"
                    && (payload
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                        || payload
                            .get("summary")
                            .and_then(Value::as_array)
                            .is_some_and(|summary| !summary.is_empty()));
                if model_message || model_reasoning {
                    return Ok(vec![WorkerEvent::ModelProgress { session_id }]);
                }
                if let Some((call_id, call)) = canonical_rollout_tool_call(item_type, payload) {
                    let session_id_for_failure = session_id.clone();
                    let event = WorkerEvent::PreToolUse {
                        session_id,
                        tool_name: call.tool_name.clone(),
                        tool_input: call.tool_input.clone(),
                    };
                    // Counted for the guard-trace cross-check: a turn that ran
                    // tool calls with zero guard invocations means the hooks
                    // did not execute.
                    self.observed_tool_calls = self.observed_tool_calls.saturating_add(1);
                    return match self.calls.observe_call(&call_id, call) {
                        Ok(CallDisposition::Announce) => Ok(vec![event]),
                        // A `wait` poll of an already-announced command. Its
                        // command was reported when the cell started, and
                        // re-announcing it would report a tool named `wait`
                        // as the worker's activity and double-count the
                        // command in every `PreToolUse` consumer — including
                        // the editorial audit, which writes a row per
                        // observed `gh pr`/`cube pr` invocation.
                        Ok(CallDisposition::Continuation) => Ok(vec![WorkerEvent::ToolPoll {
                            session_id: session_id_for_failure,
                            awaiting_model: false,
                        }]),
                        Err(err) => Ok(vec![correlation_failure_notification(&session_id_for_failure, &err)]),
                    };
                }
                if is_rollout_tool_output(item_type) {
                    let call_id = payload
                        .get("call_id")
                        .and_then(Value::as_str)
                        .ok_or(NormalizeError::MissingField("payload.call_id"))?;
                    let raw_output = payload.get("output").cloned().unwrap_or(Value::Null);
                    let call = match self.calls.observe_output(call_id, &raw_output) {
                        Ok(OutputDisposition::Completed(call) | OutputDisposition::UnconfirmedResult(call)) => call,
                        // The cell yielded: nothing about the command is
                        // observed yet, and its real output is still to come
                        // on a `wait`. Reporting the placeholder as this
                        // command's result is what made a still-running
                        // command look observed.
                        Ok(OutputDisposition::StillRunning { .. }) => {
                            return Ok(vec![WorkerEvent::ToolPoll {
                                session_id,
                                awaiting_model: true,
                            }]);
                        }
                        Err(err) => return Ok(vec![correlation_failure_notification(&session_id, &err)]),
                    };
                    let mut events = vec![WorkerEvent::PostToolUse {
                        session_id: session_id.clone(),
                        tool_name: call.tool_name.clone(),
                        tool_input: call.tool_input,
                        // Preserve the rollout dialect's exact `payload.output`
                        // value. Codex's PR capture override flattens the
                        // observed string/array forms without pretending this
                        // is stdout's `aggregated_output`.
                        tool_response: raw_output.clone(),
                    }];
                    // `reported_failure` comes only from a structured chunk
                    // that actually carries an exit code — under the cell
                    // harness that chunk is wrapped in a prose header, which
                    // `cell_aware_tool_output` peels first. The marker scan is
                    // what catches the masked-denial case either way, since a
                    // denial mid-compound-command reports exit 0 honestly.
                    if call.tool_name == "Bash" {
                        let canonical = cell_aware_tool_output(&raw_output);
                        if let Some(message) = command_denial_notification(canonical.is_error, None, &canonical.body) {
                            events.push(WorkerEvent::Notification { session_id, message });
                        }
                    }
                    return Ok(events);
                }
                Err(NormalizeError::UnknownEvent(format!("response_item/{item_type}")))
            }
            // Benign bookkeeping envelopes: internal session/turn state
            // snapshots with nothing to report as progress. Recognising them
            // explicitly (rather than falling through to the catch-all
            // below) keeps `unrecognised_envelopes` meaningful — every
            // rollout dispatch used to count these, drowning out genuinely
            // novel envelope shapes.
            "world_state" | "turn_context" => Ok(Vec::new()),
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

    fn resume_state(&self) -> Option<Value> {
        serde_json::to_value(RolloutResumeState::capture(self)).ok()
    }

    fn restore_resume_state(&mut self, state: &Value) -> Result<(), String> {
        let state: RolloutResumeState =
            serde_json::from_value(state.clone()).map_err(|err| format!("rollout resume state: {err}"))?;
        state.apply(self);
        Ok(())
    }
}

/// The part of a [`CodexRolloutProgressSession`] that a fresh session cannot
/// re-derive from the bytes it is about to read.
///
/// Every field here has a wrong default under resumption, and each one is
/// wrong in a way that is *observable* rather than cosmetic:
///
/// - `current_thread_id` is announced once, by the `session_meta` record at
///   the head of the file. Resuming past that record with `None` makes every
///   subsequent record fail [`CodexRolloutProgressSession::session_id`] and
///   the whole rest of the run normalise to nothing.
/// - `calls` holds the reader's tool-call correlation: calls whose output
///   record has not arrived yet, plus the cell-harness bindings that pair a
///   yielded cell with the `wait` that continues it. Dropping it turns the
///   matching output record into an unpaired one and loses the `PostToolUse`
///   for a tool call that really did complete.
/// - `guard_trace_line` is a read cursor into an append-only decision log. A
///   zero cursor re-announces every guard decision the run already reported.
/// - `guard_records_seen` false, with tool calls then observed, fabricates the
///   `GUARDS_SILENT_MARKER` signal — the one signal that is supposed to mean
///   Codex skipped its hooks entirely.
/// - `observed_tool_calls` and `turn_terminal` are the in-flight turn's own
///   accounting; zeroing them mid-turn mis-scopes the guard comparison to a
///   fragment of the turn.
///
/// **Every field is required on the wire, and there is deliberately no
/// builder.** Only [`Self::capture`] ever writes this type and only
/// [`Self::apply`] ever reads it, so a blanket `#[serde(default)]` would buy
/// nothing but the ability to accept a *degenerate* snapshot: `{}`, or an
/// object that lost fields to a truncated write, would deserialize into an
/// all-zero state and `restore_resume_state` would return `Ok`. `apply` would
/// then quietly install exactly the session the list above calls fatal.
/// Absent fields must therefore fail deserialization and reach the attention
/// item. The one exception is `current_thread_id`, whose `None` is a real
/// value a session genuinely holds before its `session_meta` record.
///
/// A struct literal in `capture` rather than a builder, for the same reason
/// the CLAUDE.md builder convention exempts the DB mappers: a new session
/// field that nobody captured should be a compile error, not a silent
/// default. The turn accounting and the guard cursor are grouped into their
/// own types rather than flattened here, so that property holds without the
/// type reaching the size at which the convention would demand a builder.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct RolloutResumeState {
    #[serde(default)]
    current_thread_id: Option<String>,
    /// The reader's whole correlation state, in eviction order — see
    /// [`RolloutCallSnapshot`].
    calls: RolloutCallSnapshot,
    turn: TurnResumeState,
    guards: GuardResumeState,
}

/// The in-flight turn's own accounting, which is scoped to one turn and means
/// nothing across turns — hence its own type rather than two loose fields.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct TurnResumeState {
    terminal: bool,
    observed_tool_calls: usize,
}

/// The read cursor into the guard decision log, plus whether anything has
/// been read from it. The pair is only ever meaningful together: a zero
/// cursor with `records_seen` true is a log that exists and is unread, while
/// a zero cursor with it false is the `GUARDS_SILENT_MARKER` precondition.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct GuardResumeState {
    trace_line: usize,
    records_seen: bool,
}

impl RolloutResumeState {
    fn capture(session: &CodexRolloutProgressSession) -> Self {
        Self {
            current_thread_id: session.current_thread_id.clone(),
            calls: session.calls.snapshot(),
            turn: TurnResumeState {
                terminal: session.turn_terminal,
                observed_tool_calls: session.observed_tool_calls,
            },
            guards: GuardResumeState {
                trace_line: session.guard_trace_line,
                records_seen: session.guard_records_seen,
            },
        }
    }

    fn apply(self, session: &mut CodexRolloutProgressSession) {
        session.current_thread_id = self.current_thread_id;
        session.calls.restore(self.calls);
        session.turn_terminal = self.turn.terminal;
        session.observed_tool_calls = self.turn.observed_tool_calls;
        session.guard_trace_line = self.guards.trace_line;
        session.guard_records_seen = self.guards.records_seen;
    }
}

/// Best-effort, macOS-Seatbelt-verified phrasing for a filesystem write
/// refused by Codex's OS sandbox (`--sandbox read-only` / `workspace-write`).
/// Neither exhaustive (other sandboxes, or a non-filesystem denial such as a
/// network refusal, phrase differently) nor free of false positives
/// (`Operation not permitted` is the generic EPERM string, which a command
/// can hit for reasons that have nothing to do with Boss's sandbox) — see
/// "Sandbox denials are invisible to exit status alone" in
/// `docs/designs/codex-as-a-first-class-agent-driver.md`. A match only ever
/// adds a [`WorkerEvent::Notification`]; it never blocks or retries anything.
const SUSPECTED_DENIAL_MARKERS: &[&str] = &["Operation not permitted"];

/// Build the extra `Notification` message for a `command_execution` /
/// rollout tool-output result, if any.
///
/// `reported_failure` is the dialect's own exit status when it is
/// trustworthy (a command whose own exit code was not masked by shell
/// composition). `output` is scanned for a denial marker regardless of
/// `reported_failure`: a write denied mid-compound-command reports the
/// *outer* status as success (`exit_code:0`/`status:"completed"`), so the
/// marker scan is what actually catches the case this exists for.
fn command_denial_notification(reported_failure: bool, exit_code: Option<i64>, output: &str) -> Option<String> {
    if let Some(marker) = SUSPECTED_DENIAL_MARKERS
        .iter()
        .copied()
        .find(|marker| output.contains(marker))
    {
        return Some(format!(
            "codex: command output contains a suspected sandbox/permission denial ({marker:?}); a \
             compound shell command can still report overall success, so verify no required write \
             was silently refused"
        ));
    }
    reported_failure
        .then(|| format!("codex: command execution reported a non-zero exit status (exit_code={exit_code:?})"))
}

/// Verify a legacy in-home sessions directory is a real (non-symlinked)
/// descendant of the Codex home. New provisioned homes intentionally use a
/// durable sessions link and are handled by the driver's containment method.
pub(super) fn verified_sessions_root(homes_root: &Path, codex_home: &Path) -> Option<PathBuf> {
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
    Some(canonical_sessions)
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

pub(super) struct CodexTranscriptSession {
    calls: RolloutCallTracker,
    /// Assistant response_item messages are the canonical transcript source.
    /// Keep whether one arrived since the current task boundary so its
    /// last_agent_message can remain a fallback for a partially flushed
    /// rollout without becoming a second copy of an already-recorded message.
    assistant_response_since_boundary: bool,
}

impl Default for CodexTranscriptSession {
    /// The transcript correlator lives for the whole run and is never drained
    /// at a turn boundary, so it is sized at
    /// [`MAX_TRACKED_TRANSCRIPT_TOOL_CALLS`] rather than the progress
    /// session's per-turn cap.
    fn default() -> Self {
        Self {
            calls: RolloutCallTracker::with_capacity(MAX_TRACKED_TRANSCRIPT_TOOL_CALLS, "transcript"),
            assistant_response_since_boundary: false,
        }
    }
}

impl CodexTranscriptSession {
    fn normalize(&mut self, raw: Value) -> Value {
        let timestamp = raw.get("timestamp").cloned();
        let parsed = match raw.as_object().and_then(parse_rollout_envelope) {
            Some(parsed) => parsed,
            None => {
                tracing::debug!("codex rollout: ignoring malformed/non-object transcript record");
                return raw;
            }
        };

        let normalized = match parsed {
            RolloutEnvelope::SessionMeta => {
                self.calls.reset();
                self.assistant_response_since_boundary = false;
                json!({"type":"system"})
            }
            RolloutEnvelope::EventMessage { event_type, payload } => self
                .normalize_rollout_event_message(&event_type, &payload)
                .unwrap_or_else(|| {
                    tracing::debug!(event_type, "codex rollout: ignoring additive event_msg variant");
                    raw
                }),
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
        };
        preserve_rollout_timestamp(normalized, timestamp)
    }

    fn normalize_rollout_response_item(&mut self, item_type: &str, payload: &Map<String, Value>) -> Option<Value> {
        if let Some((call_id, call)) = canonical_rollout_tool_call(item_type, payload) {
            let normalized = json!({
                "type":"assistant",
                "content":[{
                    "type":"tool_use",
                    "name":call.tool_name.clone(),
                    "input":call.tool_input.clone(),
                }]
            });
            return match self.calls.observe_call(&call_id, call) {
                Ok(CallDisposition::Announce) => Some(normalized),
                // A `wait` poll of a cell whose command is already in the
                // transcript. Rendering it would show a tool named `wait`
                // taking a `cell_id` where a reader (and every marker scan
                // over this transcript) expects the worker's commands.
                Ok(CallDisposition::Continuation) => Some(json!({"type":"system"})),
                Err(err) => {
                    tracing::warn!(call_id, %err, "codex rollout: could not correlate transcript tool call");
                    Some(json!({"type":"system"}))
                }
            };
        }
        if is_rollout_tool_output(item_type) {
            let call_id = payload.get("call_id")?.as_str()?;
            let raw_output = payload.get("output").cloned().unwrap_or(Value::Null);
            let call = match self.calls.observe_output(call_id, &raw_output) {
                Ok(OutputDisposition::Completed(call) | OutputDisposition::UnconfirmedResult(call)) => call,
                // The command is still running; its real result arrives on a
                // later `wait` and is rendered against this same call then.
                // Say so rather than emitting a bare `{"type":"system"}`: if
                // the model never polls again there is no later record at
                // all, and a reader (or a marker scan over this transcript)
                // would otherwise see a `tool_use` with no result and no
                // indication why.
                Ok(OutputDisposition::StillRunning { cell_id }) => {
                    return Some(lifecycle_system(
                        "cell_running",
                        &cell_still_running_note(cell_id.as_deref()),
                    ));
                }
                Err(err) => {
                    tracing::warn!(call_id, %err, "codex rollout: omitting uncorrelated tool output body");
                    return Some(lifecycle_system(
                        "cell_uncorrelated",
                        &format!("tool output could not be correlated: {err}"),
                    ));
                }
            };
            let output = cell_aware_tool_output(&raw_output);
            return Some(json!({
                "type":"tool_result",
                "content":output.body,
                "is_error":output.is_error,
                "tool_name":call.tool_name,
                "tool_input":call.tool_input,
            }));
        }
        if item_type != "message" {
            return None;
        }
        let normalized = normalize_rollout_message(payload)?;
        if normalized["type"] == "assistant" {
            self.assistant_response_since_boundary = true;
        }
        Some(normalized)
    }

    fn normalize_rollout_event_message(&mut self, event_type: &str, payload: &Map<String, Value>) -> Option<Value> {
        match event_type {
            // `response_item` role=assistant is the canonical per-message
            // source. `agent_message` is a rollout echo of it.
            "agent_message" => Some(json!({"type":"system"})),
            // `event_msg.user_message` is the canonical user source. Its
            // response_item counterpart includes injected boilerplate.
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
            "task_started" => {
                self.assistant_response_since_boundary = false;
                Some(lifecycle_system("task_started", "turn started"))
            }
            "task_complete" => {
                let had_canonical_assistant = self.assistant_response_since_boundary;
                self.assistant_response_since_boundary = false;

                // A fatal `error` takes priority over `last_agent_message` (Codex
                // sends `last_agent_message: null` alongside a fatal error — the
                // worker never got to speak). Surfaced as AssistantText, not a
                // lifecycle filler, so transcript readers recover the provider's
                // diagnostic instead of finding no assistant text at all.
                if let Some(message) = rollout_task_complete_error_message(payload) {
                    return Some(assistant_text(&format!("Codex reported a fatal error: {message}")));
                }
                // A partial rollout can terminate before the canonical
                // response_item flushes. Keep last_agent_message only in that
                // case; otherwise it is an unconditional duplicate.
                match (
                    had_canonical_assistant,
                    payload.get("last_agent_message").and_then(Value::as_str),
                ) {
                    (false, Some(message)) => Some(assistant_text(message)),
                    _ => Some(lifecycle_system("task_complete", "turn completed")),
                }
            }
            _ => None,
        }
    }
}

impl TranscriptSessionNormalizer for CodexTranscriptSession {
    fn normalize_transcript_entry(&mut self, raw: Value) -> Value {
        self.normalize(raw)
    }
}

fn preserve_rollout_timestamp(mut normalized: Value, timestamp: Option<Value>) -> Value {
    let Some(timestamp) = timestamp else {
        return normalized;
    };
    if let Some(object) = normalized.as_object_mut() {
        object.insert("timestamp".to_owned(), timestamp);
    }
    normalized
}

/// Extract a diagnostic message from a rollout `task_complete` payload's
/// `error` field, if present and non-null. `None` means a clean completion.
///
/// Codex's shape (verified against the field incident this closes): `error`
/// is an object carrying `message` (itself often a JSON-encoded string, e.g.
/// `{"type":"error","status":400,...}`) and `codex_error_info` (a short
/// classifier, e.g. `"other"`). Both are included when present so the
/// resulting attention item/log line names the specific failure rather than
/// a bare "codex errored".
fn rollout_task_complete_error_message(payload: &Map<String, Value>) -> Option<String> {
    let error = payload.get("error")?;
    if error.is_null() {
        return None;
    }
    let message = error.get("message").and_then(Value::as_str);
    let info = error.get("codex_error_info").and_then(Value::as_str);
    Some(match (info, message) {
        (Some(info), Some(message)) => format!("codex_error_info={info}: {message}"),
        (None, Some(message)) => message.to_owned(),
        (Some(info), None) => format!("codex_error_info={info}"),
        (None, None) => error.to_string(),
    })
}

/// Codex turn-boundary filler, tagged as system so it is not worker prose.
///
/// Marker scans and the Stop-boundary flush-race retry gate on
/// `AssistantText`. Lifecycle placeholders must not count: a synthetic
/// "turn started" on a partial rollout used to make `all_text` non-empty and
/// disable the retry, permanently dropping a late-flushed `[blocked]` marker.
/// The transcript note standing in for a command whose result has not been
/// delivered yet.
fn cell_still_running_note(cell_id: Option<&str>) -> String {
    match cell_id {
        Some(cell_id) => format!("command still running; no result delivered yet (cell {cell_id})"),
        None => "command still running; no result delivered yet".to_owned(),
    }
}

/// Turn a correlation failure into an operator-visible event.
///
/// A `NormalizeError` alone does not reach an operator: the production
/// reader (`boss_engine_stdout_progress`) treats *any* `Err` from
/// `normalize_progress_events` as the expected steady state — it bumps
/// `unrecognised_envelopes`, logs at `debug!`, and skips the line. A `wait`
/// naming a cell this session never saw yield would then be dropped exactly
/// as silently as the bug this correlation exists to expose, PR URL and all.
///
/// [`super::UNOBSERVED_COMMAND_MARKER`] is the right channel because it says
/// precisely what happened: Boss watched a command's records go past and
/// could not confirm its outcome. The engine files an attention item and
/// stops treating the run's `no-changes-needed` claim as confirmed.
fn correlation_failure_notification(session_id: &str, err: &CorrelationError) -> WorkerEvent {
    tracing::warn!(%err, "codex rollout: correlation failure surfaced as an unobserved command");
    WorkerEvent::Notification {
        session_id: session_id.to_owned(),
        message: super::unobserved_command_notification(&err.lost_command_display()),
    }
}

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
        "assistant" => Some(if text.is_empty() {
            json!({"type":"system"})
        } else {
            assistant_text(&text)
        }),
        // `event_msg.user_message` is the canonical source for user prose;
        // it avoids replaying Codex's injected context. Developer/system
        // messages are likewise not conversation turns. Return the canonical
        // system marker so deliberate suppression is distinct from an
        // unrecognized additive rollout variant.
        "user" | "developer" | "system" => Some(json!({"type":"system"})),
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
#[path = "progress_tests.rs"]
mod tests;
