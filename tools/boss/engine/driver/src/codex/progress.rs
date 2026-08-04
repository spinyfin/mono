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
    CanonicalRolloutToolCall, canonical_rollout_tool_call as shared_canonical_rollout_tool_call,
    canonical_rollout_tool_output, extract_text_blocks, is_rollout_tool_output,
};
use boss_protocol::{NormalizeError, SessionStartSource, StopReason, WorkerEvent};
use serde_json::{Map, Value, json};

use super::guard_chain::{self, ArmedChainStatus};
use super::guard_trace;
use super::rollout_calls::{
    CallDisposition, CorrelationError, MAX_TRACKED_TRANSCRIPT_TOOL_CALLS, OutputDisposition, RolloutCallSnapshot,
    RolloutCallTracker, RolloutToolCall, cell_aware_tool_output, rollout_call_command_display,
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
    } = shared_canonical_rollout_tool_call(item_type, payload)?;
    let call_id = call_id?;
    Some((call_id, RolloutToolCall { tool_name, tool_input }))
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
    /// [`super::UNOBSERVED_COMMAND_MARKER`], oldest-first. Scoped to
    /// exec/shell-shaped calls only — those whose `tool_input` carries a
    /// `command` field, matching what [`rollout_call_command_display`]
    /// prefers and mirroring the stdout dialect, which only tracks
    /// `command_execution`. A pending non-shell call (`apply_patch`, file
    /// reads, MCP tools) is dropped from tracking without a notification;
    /// it never gates `NO_CHANGES_NEEDED`. Called immediately before a
    /// `Stop` is emitted so a later, unrelated turn never re-flags the same
    /// call.
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
            .filter(|call| call.tool_input.get("command").and_then(Value::as_str).is_some())
            .map(|call| WorkerEvent::Notification {
                session_id: session_id.to_owned(),
                message: super::unobserved_command_notification(&rollout_call_command_display(&call)),
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
                        Ok(CallDisposition::Continuation) => Ok(Vec::new()),
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
                        Ok(OutputDisposition::Completed(call)) => call,
                        // The cell yielded: nothing about the command is
                        // observed yet, and its real output is still to come
                        // on a `wait`. Reporting the placeholder as this
                        // command's result is what made a still-running
                        // command look observed.
                        Ok(OutputDisposition::StillRunning { .. }) => return Ok(Vec::new()),
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

/// Verify `codex_home` is a real (non-symlinked) descendant of `homes_root`
/// and return its canonical `sessions` subdirectory, itself a real
/// descendant of `codex_home` — or `None` if any link in that chain is
/// symlinked or doesn't nest as expected.
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
                Ok(OutputDisposition::Completed(call)) => call,
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
            let output = canonical_rollout_tool_output(&raw_output);
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
/// stops treating the run's `NO_CHANGES_NEEDED` claim as confirmed.
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
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn rollout_prose_wrapped_denial_adds_a_suspected_denial_notification() {
        // Empirical capture, rollout dialect: the exec_command tool's own
        // textual wrapper embeds "Process exited with code 0" as prose while
        // the denial itself is buried further inside that same string.
        let mut session = CodexRolloutProgressSession::new(None, None, None);
        session
            .normalize_progress_events(&json!({
                "type":"session_meta",
                "payload":{"id":"thread-rollout-denied","cwd":"/tmp/workspace"}
            }))
            .unwrap();
        session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "name":"exec_command",
                    "call_id":"call-denied",
                    "arguments":r#"{"cmd":"touch denied.txt; echo \"exit:$?\""}"#
                }
            }))
            .unwrap();
        let events = session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call_output",
                    "call_id":"call-denied",
                    "output":"Process exited with code 0\nOutput:\ntouch: denied.txt: Operation not permitted\nexit:1\n"
                }
            }))
            .unwrap();
        assert!(matches!(&events[0], WorkerEvent::PostToolUse { .. }), "got {events:?}");
        assert!(
            matches!(&events[1], WorkerEvent::Notification { message, .. } if message.contains("suspected sandbox/permission denial")),
            "got {events:?}"
        );
    }

    #[test]
    fn rollout_structured_exit_code_adds_a_notification_without_the_marker_text() {
        let mut session = CodexRolloutProgressSession::new(None, None, None);
        session
            .normalize_progress_events(&json!({
                "type":"session_meta",
                "payload":{"id":"thread-rollout-failed","cwd":"/tmp/workspace"}
            }))
            .unwrap();
        session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call",
                    "name":"exec",
                    "call_id":"call-failed",
                    "input":"some-tool that fails"
                }
            }))
            .unwrap();
        let events = session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call_output",
                    "call_id":"call-failed",
                    "output":"{\"output\":\"boom\\n\",\"metadata\":{\"exit_code\":1}}"
                }
            }))
            .unwrap();
        assert!(matches!(&events[0], WorkerEvent::PostToolUse { .. }), "got {events:?}");
        assert!(
            matches!(&events[1], WorkerEvent::Notification { message, .. } if message.contains("non-zero exit status")),
            "got {events:?}"
        );
    }

    #[test]
    fn rollout_clean_output_adds_no_extra_notification() {
        let mut session = CodexRolloutProgressSession::new(None, None, None);
        session
            .normalize_progress_events(&json!({
                "type":"session_meta",
                "payload":{"id":"thread-rollout-clean","cwd":"/tmp/workspace"}
            }))
            .unwrap();
        session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call",
                    "name":"exec",
                    "call_id":"call-clean",
                    "input":"echo hi"
                }
            }))
            .unwrap();
        let events = session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call_output",
                    "call_id":"call-clean",
                    "output":[{"type":"input_text","text":"hi\n"}]
                }
            }))
            .unwrap();
        assert_eq!(events.len(), 1, "got {events:?}");
    }

    /// Drive one rollout turn that runs a single code-mode cell, and return the
    /// events emitted at the turn boundary.
    fn guard_trace_turn(session: &mut CodexRolloutProgressSession, call_id: &str) -> Vec<WorkerEvent> {
        session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call",
                    "name":"exec",
                    "call_id":call_id,
                    "input":"text('hello');\n"
                }
            }))
            .unwrap();
        session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call_output",
                    "call_id":call_id,
                    "output":[{"type":"input_text","text":"hello\n"}]
                }
            }))
            .unwrap();
        let events = session
            .normalize_progress_events(&json!({"type":"event_msg","payload":{"type":"task_complete"}}))
            .unwrap();
        session
            .normalize_progress_events(&json!({"type":"event_msg","payload":{"type":"task_started"}}))
            .unwrap();
        events
    }

    /// A `CODEX_HOME` shaped like one `write_hooks_and_attest` leaves behind: a
    /// config that declares and trusts one hook, an executable wrapper, and an
    /// attestation binding its bytes. Returns the wrapper so a test can break
    /// the chain the way a live session can.
    fn armed_codex_home(home: &Path) -> PathBuf {
        use boss_engine_codex_hook_trust::{
            HookAttestationEntry, HookTrustAttestation, ObservationProof, sha256_hex_prefixed, write_attestation_file,
        };

        let guards = home.join("guards");
        std::fs::create_dir_all(&guards).unwrap();
        let wrapper = guards.join("00_path_guard.sh");
        let body = "#!/bin/sh\nexit 0\n";
        std::fs::write(&wrapper, body).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[[hooks.PreToolUse]]\n\
                 matcher = \".*\"\n\
                 [[hooks.PreToolUse.hooks]]\n\
                 type = \"command\"\n\
                 command = \"{}\"\n\
                 \n\
                 [hooks.state.\"k\"]\n\
                 trusted_hash = \"sha256:whatever\"\n",
                wrapper.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let attestation = HookTrustAttestation {
            codex_home: home.display().to_string(),
            config_path: home.join("config.toml").display().to_string(),
            generated_at_unix: 0,
            hooks: vec![
                HookAttestationEntry::builder()
                    .key("k")
                    .event("pre_tool_use")
                    .command(wrapper.display().to_string())
                    .matcher(".*")
                    .trusted_hash("sha256:whatever")
                    .guard_content_sha256(sha256_hex_prefixed(body.as_bytes()))
                    .observed_trust_status("trusted")
                    .build(),
            ],
            observation: ObservationProof::HooksList { codex_version: None },
        };
        write_attestation_file(&super::super::guard_chain::attestation_path(home), &attestation).unwrap();
        wrapper
    }

    fn started_rollout_session(home: &Path) -> CodexRolloutProgressSession {
        let mut session = CodexRolloutProgressSession::new(None, None, None).with_codex_home(Some(home.to_path_buf()));
        session
            .normalize_progress_events(&json!({
                "type":"session_meta",
                "payload":{"id":"thread-guards","cwd":"/tmp/workspace"}
            }))
            .unwrap();
        session
    }

    fn is_silent_signal(events: &[WorkerEvent]) -> bool {
        events.iter().any(|event| {
            matches!(event, WorkerEvent::Notification { message, .. }
                if message.starts_with(super::super::GUARDS_SILENT_MARKER))
        })
    }

    #[test]
    fn a_turn_with_tool_calls_and_no_guard_record_anywhere_reports_guards_silent() {
        let dir = TempDir::new().unwrap();
        armed_codex_home(dir.path());
        let mut session = started_rollout_session(dir.path());

        let events = guard_trace_turn(&mut session, "call-1");
        assert!(is_silent_signal(&events), "got {events:?}");
    }

    #[test]
    fn a_quiet_turn_after_guards_have_been_seen_reports_nothing() {
        // The structural false positive this closes: `observed_tool_calls`
        // counts every rollout `custom_tool_call`, which on a code-mode model is
        // the JavaScript cell itself. A cell that invokes no inner tool fires no
        // PreToolUse at all, so a turn that only computes or prints yielded
        // observed >= 1 with zero guard records and logged "command guardrails
        // were not enforced" at error. With the chain re-verified intact this
        // turn, one earlier record is enough to explain the quiet.
        let dir = TempDir::new().unwrap();
        armed_codex_home(dir.path());
        std::fs::write(
            dir.path().join(guard_trace::GUARD_TRACE_FILENAME),
            "{\"guard\":\"01_boss_launch_guard\",\"decision\":\"approve\"}\n",
        )
        .unwrap();
        let mut session = started_rollout_session(dir.path());

        // First turn: the guard record is read and summarised.
        let first = guard_trace_turn(&mut session, "call-1");
        assert!(
            first
                .iter()
                .any(|event| matches!(event, WorkerEvent::Notification { message, .. }
                    if message.starts_with(super::super::GUARD_TRACE_MARKER))),
            "got {first:?}"
        );

        // Second turn: one cell ran, it called no tool, so no new guard record
        // exists. That must be silence, not a guardrail alarm.
        let second = guard_trace_turn(&mut session, "call-2");
        assert!(
            !is_silent_signal(&second),
            "a turn whose cell invoked no tool must not report disarmed guards: {second:?}"
        );
        assert_eq!(
            second.len(),
            1,
            "only the Stop should remain for a quiet turn: {second:?}"
        );
    }

    /// The verbatim `exec` cell script from the reported failure's shape: a
    /// `cube pr create` whose cold `repobin` build pushes it past the cell's
    /// yield window.
    const PR_CELL_SCRIPT: &str = concat!(
        r#"const r = await tools.exec_command({"cmd":"cube pr create --branch boss/exec_1 --title T","#,
        r#""workdir":"/ws","yield_time_ms":30000,"max_output_tokens":2000});"#,
        "\ntext(JSON.stringify(r));"
    );

    fn cell_harness_session() -> CodexRolloutProgressSession {
        let mut session = CodexRolloutProgressSession::new(None, None, None);
        session
            .normalize_progress_events(&json!({
                "type":"session_meta",
                "payload":{"id":"thread-cell","cwd":"/ws"}
            }))
            .unwrap();
        session
    }

    fn cell_exec_call(call_id: &str, script: &str) -> Value {
        json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "name":"exec",
                "call_id":call_id,
                "input":script
            }
        })
    }

    fn cell_yield_output(call_id: &str, cell_id: u32) -> Value {
        json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":call_id,
                "output":format!("Script running with cell ID {cell_id}\nWall time 11.1 seconds\nOutput:\n")
            }
        })
    }

    fn cell_wait_call(call_id: &str, cell_id: u32) -> Value {
        json!({
            "type":"response_item",
            "payload":{
                "type":"function_call",
                "name":"wait",
                "call_id":call_id,
                "arguments":format!("{{\"cell_id\":\"{cell_id}\",\"yield_time_ms\":30000,\"max_tokens\":2000}}")
            }
        })
    }

    fn cell_completed_output(call_id: &str, payload: &str) -> Value {
        json!({
            "type":"response_item",
            "payload":{
                "type":"function_call_output",
                "call_id":call_id,
                "output":[
                    {"type":"input_text","text":"Script completed\nWall time 17.7 seconds\nOutput:\n"},
                    {"type":"input_text","text":payload}
                ]
            }
        })
    }

    #[test]
    fn cell_harness_exec_announces_the_shell_command_not_the_script() {
        let mut session = cell_harness_session();
        let events = session
            .normalize_progress_events(&cell_exec_call("call-pr", PR_CELL_SCRIPT))
            .unwrap();
        assert_eq!(
            events,
            vec![WorkerEvent::PreToolUse {
                session_id: "thread-cell".into(),
                tool_name: "Bash".into(),
                tool_input: json!({"command":"cube pr create --branch boss/exec_1 --title T"}),
            }],
            "the whole JavaScript cell must not render as the command"
        );
    }

    #[test]
    fn a_yielded_cells_wait_continuation_carries_the_command_and_its_real_output() {
        // The reported failure, replayed: `cube pr create` outlives the
        // cell's yield window, so the call's own output is only a
        // placeholder and the URL arrives on the `wait`.
        let mut session = cell_harness_session();
        session
            .normalize_progress_events(&cell_exec_call("call-pr", PR_CELL_SCRIPT))
            .unwrap();

        assert_eq!(
            session
                .normalize_progress_events(&cell_yield_output("call-pr", 1))
                .unwrap(),
            Vec::new(),
            "a yield placeholder is not this command's result and must not be reported as one"
        );
        assert_eq!(
            session
                .normalize_progress_events(&cell_wait_call("call-wait", 1))
                .unwrap(),
            Vec::new(),
            "a wait poll re-announces nothing; the command was announced when its cell started"
        );

        let chunk = r#"{"chunk_id":"ab","exit_code":0,"output":"https://github.com/spinyfin/mono/pull/9\n"}"#;
        let events = session
            .normalize_progress_events(&cell_completed_output("call-wait", chunk))
            .unwrap();
        assert_eq!(
            events,
            vec![WorkerEvent::PostToolUse {
                session_id: "thread-cell".into(),
                tool_name: "Bash".into(),
                tool_input: json!({"command":"cube pr create --branch boss/exec_1 --title T"}),
                tool_response: json!([
                    {"type":"input_text","text":"Script completed\nWall time 17.7 seconds\nOutput:\n"},
                    {"type":"input_text","text":chunk}
                ]),
            }],
            "the completion must be attributed to the originating command, not to the wait"
        );
    }

    /// Probe 6's `wait` output verbatim: the cell completed, but its chunk
    /// carries `session_id` and no `exit_code` and only 8 of the command's
    /// 12 ticks arrived — the command was still running.
    const P6_RUNNING_CHUNK: &str = concat!(
        r#"{"chunk_id":"d0540d","wall_time_seconds":30.001035083,"session_id":8467,"#,
        r#""original_token_count":14,"output":"tick-1\ntick-2\ntick-3\ntick-4\ntick-5\ntick-6\ntick-7\ntick-8\n"}"#
    );

    #[test]
    fn probe_6_replayed_end_to_end_flags_the_command_the_model_stopped_polling() {
        // The four records of `p6_hidden_exit`'s rollout, in order. The
        // `wait` comes back `Script completed` — a claim about the cell, not
        // the command — with a chunk carrying no exit code. Treating that as
        // terminal is what made the reported failure invisible: the call
        // closed, a clean `PostToolUse` went out, and the abandoned-command
        // guard never fired.
        let mut session = cell_harness_session();
        const P6_SCRIPT: &str = concat!(
            r#"const r = await tools.exec_command({"cmd":"sh -c 'for i in $(seq 1 12); do echo tick-$i; "#,
            r#"sleep 4; done; echo FINAL-LINE; exit 4'","yield_time_ms":30000});"#,
            "\ntext(JSON.stringify(r));"
        );
        const P6_COMMAND: &str =
            "sh -c 'for i in $(seq 1 12); do echo tick-$i; sleep 4; done; echo FINAL-LINE; exit 4'";

        session
            .normalize_progress_events(&cell_exec_call("call-p6", P6_SCRIPT))
            .unwrap();
        session
            .normalize_progress_events(&cell_yield_output("call-p6", 1))
            .unwrap();
        session
            .normalize_progress_events(&cell_wait_call("call-p6-wait", 1))
            .unwrap();
        assert_eq!(
            session
                .normalize_progress_events(&cell_completed_output("call-p6-wait", P6_RUNNING_CHUNK))
                .unwrap(),
            Vec::new(),
            "a chunk with no exit code is not this command's result and must not be reported as one"
        );

        // The model answers `observed_exit=NONE` and the turn ends.
        let events = session
            .normalize_progress_events(&json!({
                "type":"event_msg",
                "payload":{"type":"task_complete","last_agent_message":"observed_exit=NONE","error":null}
            }))
            .unwrap();
        assert_eq!(
            events,
            vec![
                WorkerEvent::Notification {
                    session_id: "thread-cell".into(),
                    message: crate::codex::unobserved_command_notification(P6_COMMAND),
                },
                WorkerEvent::Stop {
                    session_id: "thread-cell".into(),
                    stop_hook_active: false,
                    stop_reason: StopReason::Completed,
                },
            ],
            "the command the model stopped polling must be flagged unobserved"
        );
    }

    #[test]
    fn a_cell_that_completes_with_an_exit_code_is_terminal_for_the_command() {
        // Probe 4's *second* poll: the same envelope header as probe 6's,
        // but the chunk carries `exit_code`. That, not `Script completed`,
        // is the command's terminal signal.
        let mut session = cell_harness_session();
        session
            .normalize_progress_events(&cell_exec_call("call-p4", PR_CELL_SCRIPT))
            .unwrap();
        session
            .normalize_progress_events(&cell_yield_output("call-p4", 2))
            .unwrap();
        session
            .normalize_progress_events(&cell_wait_call("call-p4-wait", 2))
            .unwrap();
        let events = session
            .normalize_progress_events(&cell_completed_output(
                "call-p4-wait",
                r#"{"chunk_id":"5ec81c","exit_code":4,"output":"tick-12\nFINAL-LINE\n"}"#,
            ))
            .unwrap();
        assert!(matches!(&events[0], WorkerEvent::PostToolUse { .. }), "got {events:?}");
        let stop = session
            .normalize_progress_events(&json!({
                "type":"event_msg",
                "payload":{"type":"task_complete","last_agent_message":"done","error":null}
            }))
            .unwrap();
        assert!(
            matches!(stop.as_slice(), [WorkerEvent::Stop { .. }]),
            "an observed command must not be flagged abandoned, got {stop:?}"
        );
    }

    #[test]
    fn a_yielded_cell_that_is_never_polled_is_flagged_abandoned_at_the_turn_boundary() {
        let mut session = cell_harness_session();
        session
            .normalize_progress_events(&cell_exec_call("call-slow", PR_CELL_SCRIPT))
            .unwrap();
        session
            .normalize_progress_events(&cell_yield_output("call-slow", 1))
            .unwrap();

        // The model stops polling and ends the turn — probe 6's abandon shape.
        let events = session
            .normalize_progress_events(&json!({
                "type":"event_msg",
                "payload":{"type":"task_complete","last_agent_message":"done","error":null}
            }))
            .unwrap();
        assert_eq!(
            events,
            vec![
                WorkerEvent::Notification {
                    session_id: "thread-cell".into(),
                    message: crate::codex::unobserved_command_notification(
                        "cube pr create --branch boss/exec_1 --title T"
                    ),
                },
                WorkerEvent::Stop {
                    session_id: "thread-cell".into(),
                    stop_hook_active: false,
                    stop_reason: StopReason::Completed,
                },
            ]
        );
    }

    #[test]
    fn a_cell_harness_exit_code_is_no_longer_read_as_a_clean_command() {
        // Probe 1's shape end to end: the chunk's top-level `exit_code` sits
        // behind the harness's prose header, which used to make every Codex
        // command read as a non-error.
        let mut session = cell_harness_session();
        session
            .normalize_progress_events(&cell_exec_call(
                "call-fail",
                r#"await tools.exec_command({"cmd":"sh -c 'exit 7'"});"#,
            ))
            .unwrap();
        let events = session
            .normalize_progress_events(&cell_completed_output(
                "call-fail",
                r#"{"chunk_id":"66d4c6","exit_code":7,"output":"LINE-ONE\n"}"#,
            ))
            .unwrap();
        assert!(matches!(&events[0], WorkerEvent::PostToolUse { .. }), "got {events:?}");
        assert!(
            matches!(&events[1], WorkerEvent::Notification { message, .. } if message.contains("non-zero exit status")),
            "got {events:?}"
        );
    }

    #[test]
    fn a_wait_naming_an_unknown_cell_reaches_an_operator() {
        // A `NormalizeError` alone would not: the production reader treats
        // every `Err` as the expected steady state, bumps a counter and logs
        // at `debug!`. The failure has to travel on a channel the engine
        // actually surfaces, so it files an attention item instead.
        let mut session = cell_harness_session();
        let events = session
            .normalize_progress_events(&cell_wait_call("call-wait", 4))
            .unwrap();
        let [WorkerEvent::Notification { session_id, message }] = events.as_slice() else {
            panic!("an uncorrelatable continuation must not look like a clean no-op: {events:?}");
        };
        assert_eq!(session_id, "thread-cell");
        assert!(
            message.starts_with(crate::codex::UNOBSERVED_COMMAND_MARKER),
            "{message}"
        );
        assert!(message.contains("cell 4"), "{message}");
    }

    #[test]
    fn an_output_for_an_untracked_call_reaches_an_operator() {
        let mut session = cell_harness_session();
        let events = session
            .normalize_progress_events(&cell_completed_output("ghost", r#"{"exit_code":0,"output":"x"}"#))
            .unwrap();
        let [WorkerEvent::Notification { message, .. }] = events.as_slice() else {
            panic!("an unattributable tool output must not look like a clean no-op: {events:?}");
        };
        assert!(
            message.starts_with(crate::codex::UNOBSERVED_COMMAND_MARKER),
            "{message}"
        );
        assert!(message.contains("ghost"), "{message}");
    }

    #[test]
    fn a_new_session_cannot_resolve_a_previous_sessions_cell() {
        let mut session = cell_harness_session();
        session
            .normalize_progress_events(&cell_exec_call("call-pr", PR_CELL_SCRIPT))
            .unwrap();
        session
            .normalize_progress_events(&cell_yield_output("call-pr", 1))
            .unwrap();
        session
            .normalize_progress_events(&json!({
                "type":"session_meta",
                "payload":{"id":"thread-cell-2","cwd":"/ws"}
            }))
            .unwrap();
        let events = session
            .normalize_progress_events(&cell_wait_call("call-wait", 1))
            .unwrap();
        let [WorkerEvent::Notification { session_id, message }] = events.as_slice() else {
            panic!("a stale cell must not resolve silently: {events:?}");
        };
        assert_eq!(session_id, "thread-cell-2");
        assert!(
            message.starts_with(crate::codex::UNOBSERVED_COMMAND_MARKER),
            "{message}"
        );
    }

    #[test]
    fn transcript_rendering_correlates_the_wait_continuation_to_its_command() {
        let mut session = CodexTranscriptSession::default();
        assert_eq!(
            session.normalize_transcript_entry(cell_exec_call("call-pr", PR_CELL_SCRIPT)),
            json!({
                "type":"assistant",
                "content":[{
                    "type":"tool_use",
                    "name":"Bash",
                    "input":{"command":"cube pr create --branch boss/exec_1 --title T"},
                }]
            })
        );
        assert_eq!(
            session.normalize_transcript_entry(cell_yield_output("call-pr", 1)),
            lifecycle_system(
                "cell_running",
                "command still running; no result delivered yet (cell 1)"
            ),
            "a yield placeholder is not a tool result, and must say why there is none yet"
        );
        assert_eq!(
            session.normalize_transcript_entry(cell_wait_call("call-wait", 1)),
            json!({"type":"system"}),
            "a wait poll must not render as a tool named `wait`"
        );
        let rendered = session.normalize_transcript_entry(cell_completed_output("call-wait", "done\n"));
        assert_eq!(rendered.get("type").and_then(Value::as_str), Some("tool_result"));
        assert!(
            rendered
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| content.ends_with("done\n")),
            "completed cell output must be preserved: {rendered}"
        );
        assert_eq!(
            rendered.get("tool_input"),
            Some(&json!({"command":"cube pr create --branch boss/exec_1 --title T"}))
        );
    }

    #[test]
    fn a_command_whose_result_never_arrives_says_so_in_the_transcript() {
        // Probe 6 in the transcript: the assistant `tool_use` is followed by
        // a poll that still does not deliver a result, and the model stops.
        // There is no turn-boundary drain here, so this note is the only
        // thing a reader (or a marker scan) has to explain the missing
        // result.
        let mut session = CodexTranscriptSession::default();
        session.normalize_transcript_entry(cell_exec_call("call-p6", PR_CELL_SCRIPT));
        session.normalize_transcript_entry(cell_yield_output("call-p6", 1));
        session.normalize_transcript_entry(cell_wait_call("call-p6-wait", 1));
        assert_eq!(
            session.normalize_transcript_entry(cell_completed_output("call-p6-wait", P6_RUNNING_CHUNK)),
            lifecycle_system(
                "cell_running",
                "command still running; no result delivered yet (cell 1)"
            ),
            "a `Script completed` with no exit code is not the command's result"
        );
    }

    /// The readoption contract for this session's per-run state.
    ///
    /// A session rebuilt from [`ProgressSessionNormalizer::resume_state`] must
    /// carry the guard-trace read cursor and the "guards have been seen"
    /// fact forward. Both are read cursors over an append-only log that the
    /// rollout bytes themselves say nothing about, so a fresh session cannot
    /// re-derive either one — it can only re-announce what the run already
    /// reported.
    #[test]
    fn a_resumed_session_does_not_re_announce_guard_decisions_it_already_read() {
        let dir = TempDir::new().unwrap();
        armed_codex_home(dir.path());
        std::fs::write(
            dir.path().join(guard_trace::GUARD_TRACE_FILENAME),
            "{\"guard\":\"01_boss_launch_guard\",\"decision\":\"approve\"}\n",
        )
        .unwrap();

        // The pre-restart engine: one turn, whose boundary reads and reports
        // the one guard record.
        let mut before = started_rollout_session(dir.path());
        let first = guard_trace_turn(&mut before, "call-1");
        assert!(
            first
                .iter()
                .any(|event| matches!(event, WorkerEvent::Notification { message, .. }
                    if message.starts_with(super::super::GUARD_TRACE_MARKER))),
            "precondition — the first turn reports the guard record: {first:?}"
        );
        let state = before.resume_state().expect("a rollout session snapshots its state");

        // The post-restart engine: a brand new session over the same run,
        // re-seeded from that snapshot rather than from zero.
        let mut after =
            CodexRolloutProgressSession::new(None, None, None).with_codex_home(Some(dir.path().to_path_buf()));
        after.restore_resume_state(&state).unwrap();

        let second = guard_trace_turn(&mut after, "call-2");
        assert!(
            !second
                .iter()
                .any(|event| matches!(event, WorkerEvent::Notification { message, .. }
                    if message.starts_with(super::super::GUARD_TRACE_MARKER))),
            "a restart must not re-report a guard decision the run already reported: {second:?}"
        );
        assert!(
            !second
                .iter()
                .any(|event| matches!(event, WorkerEvent::Notification { message, .. }
                    if message.starts_with(super::super::GUARDS_SILENT_MARKER))),
            "guards were seen before the restart, so the first post-restart turn must not claim \
             they were inert: {second:?}"
        );
        assert_eq!(second.len(), 1, "only the Stop should remain: {second:?}");
    }

    /// The negative control for the test above: without the restore, the same
    /// fresh session re-announces the decision and mis-reports the run. This
    /// is what a readoption that rebuilt the session from nothing would do.
    #[test]
    fn an_unrestored_session_re_announces_guard_decisions_and_proves_the_restore_matters() {
        let dir = TempDir::new().unwrap();
        armed_codex_home(dir.path());
        std::fs::write(
            dir.path().join(guard_trace::GUARD_TRACE_FILENAME),
            "{\"guard\":\"01_boss_launch_guard\",\"decision\":\"approve\"}\n",
        )
        .unwrap();
        let mut before = started_rollout_session(dir.path());
        let _ = guard_trace_turn(&mut before, "call-1");

        // What a from-scratch re-attachment looks like: the session_meta
        // record at the head of the file is read a second time (which is its
        // own duplicate `SessionStart`), and nothing carries the guard cursor.
        let mut after = started_rollout_session(dir.path());
        let second = guard_trace_turn(&mut after, "call-2");
        assert!(
            second
                .iter()
                .any(|event| matches!(event, WorkerEvent::Notification { message, .. }
                    if message.starts_with(super::super::GUARD_TRACE_MARKER))),
            "the already-read guard record is reported a second time: {second:?}"
        );
    }

    /// A tool call whose output record has not arrived yet is correlation the
    /// rollout carries only once. Losing it across a restart turns the
    /// matching `function_call_output` into an unpaired record and silently
    /// drops a `PostToolUse` for a tool call that really did complete.
    #[test]
    fn a_resumed_session_still_pairs_a_tool_call_that_started_before_the_restart() {
        let mut before = CodexRolloutProgressSession::new(None, None, None);
        before
            .normalize_progress_events(&json!({
                "type":"session_meta",
                "payload":{"id":"thread-split","cwd":"/tmp/workspace"}
            }))
            .unwrap();
        before
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "name":"exec_command",
                    "call_id":"call-split",
                    "arguments":r#"{"cmd":"printf hi"}"#
                }
            }))
            .unwrap();
        let state = before.resume_state().unwrap();

        let mut after = CodexRolloutProgressSession::new(None, None, None);
        after.restore_resume_state(&state).unwrap();
        let events = after
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call_output",
                    "call_id":"call-split",
                    "output":"hi\n"
                }
            }))
            .unwrap();
        assert!(
            matches!(
                events.as_slice(),
                [WorkerEvent::PostToolUse { session_id, tool_name, .. }]
                    if session_id == "thread-split" && tool_name == "Bash"
            ),
            "the restored session must still know both the thread and the open call: {events:?}"
        );
    }

    /// A snapshot that lost fields — a truncated write, an older shape — must
    /// fail deserialization rather than default its way to an all-zero
    /// session. Accepting `{}` would hand `apply` exactly the state the type's
    /// own doc calls fatal (no thread id, a zero guard cursor, no guard
    /// records seen) and report `Ok` while doing it, so the caller's loud
    /// path — the attention item — would never fire.
    #[test]
    fn a_degenerate_resume_snapshot_is_rejected_rather_than_defaulted() {
        let mut session = CodexRolloutProgressSession::new(None, None, None);
        let err = session
            .restore_resume_state(&json!({}))
            .expect_err("an empty object is not a session snapshot");
        assert!(err.contains("rollout resume state"), "got {err}");

        // A real snapshot with one field dropped is the truncation case, and
        // must be refused for the same reason.
        let mut full = CodexRolloutProgressSession::new(None, None, None);
        full.normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-partial","cwd":"/tmp/workspace"}
        }))
        .unwrap();
        let mut state = full.resume_state().unwrap();
        state
            .as_object_mut()
            .expect("the snapshot is an object")
            .remove("guards")
            .expect("precondition: the field was there to drop");
        assert!(
            session.restore_resume_state(&state).is_err(),
            "a snapshot missing a field must not silently resume with that field zeroed",
        );

        // `current_thread_id` is the one genuine exception: `None` is a state
        // a session really holds, before its `session_meta` record.
        let mut without_thread = full.resume_state().unwrap();
        without_thread
            .as_object_mut()
            .unwrap()
            .remove("current_thread_id")
            .unwrap();
        session
            .restore_resume_state(&without_thread)
            .expect("an absent thread id is a meaningful value, not a lost field");
    }

    #[test]
    fn a_chain_broken_after_guards_were_seen_still_reports_guards_silent() {
        // The session-lifetime defect, measured live on codex-cli 0.145.0: a
        // TUI session whose `$CODEX_HOME/guards` was removed between two turns
        // ran turn 2's shell command with zero guard records, and Codex said
        // nothing. `guard_records_seen` was already latched by turn 1, so the
        // one signal that names this condition stayed silent for good.
        let dir = TempDir::new().unwrap();
        armed_codex_home(dir.path());
        std::fs::write(
            dir.path().join(guard_trace::GUARD_TRACE_FILENAME),
            "{\"guard\":\"01_boss_launch_guard\",\"decision\":\"approve\"}\n",
        )
        .unwrap();
        let mut session = started_rollout_session(dir.path());

        let first = guard_trace_turn(&mut session, "call-1");
        assert!(!is_silent_signal(&first), "turn 1 is guarded: {first:?}");

        std::fs::remove_dir_all(dir.path().join("guards")).unwrap();

        let second = guard_trace_turn(&mut session, "call-2");
        assert!(
            is_silent_signal(&second),
            "a chain removed mid-session must still be reported: {second:?}"
        );

        // And it keeps firing: a guardrail that is still inert on the next turn
        // is still a defect, not a one-shot notice.
        let third = guard_trace_turn(&mut session, "call-3");
        assert!(is_silent_signal(&third), "got {third:?}");
    }

    #[test]
    fn a_broken_chain_does_not_suppress_the_surviving_guards_trace() {
        // `verify_armed_chain_on_disk` stops at the first bad entry, so one
        // wrapper removed of several leaves the rest live and recording. Their
        // decisions — a block, a guard error — are exactly what an operator
        // needs while the chain is degraded, so the turn must report both.
        let dir = TempDir::new().unwrap();
        armed_codex_home(dir.path());
        let trace = dir.path().join(guard_trace::GUARD_TRACE_FILENAME);
        std::fs::write(
            &trace,
            "{\"guard\":\"01_boss_launch_guard\",\"decision\":\"approve\"}\n",
        )
        .unwrap();
        let mut session = started_rollout_session(dir.path());

        let first = guard_trace_turn(&mut session, "call-1");
        assert!(!is_silent_signal(&first), "turn 1 is guarded: {first:?}");

        std::fs::remove_dir_all(dir.path().join("guards")).unwrap();
        std::fs::write(
            &trace,
            "{\"guard\":\"01_boss_launch_guard\",\"decision\":\"approve\"}\n\
             {\"guard\":\"02_push_guard\",\"decision\":\"block\",\"reason\":\"jj git push\"}\n",
        )
        .unwrap();

        let second = guard_trace_turn(&mut session, "call-2");
        assert!(
            is_silent_signal(&second),
            "the broken chain must be reported: {second:?}"
        );
        let trace_summary = second
            .iter()
            .find_map(|event| match event {
                WorkerEvent::Notification { message, .. } if message.starts_with(super::super::GUARD_TRACE_MARKER) => {
                    Some(message.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("the surviving guards' decisions must still be reported: {second:?}"));
        assert!(
            trace_summary.contains("jj git push"),
            "the block a live guard recorded must reach the operator: {trace_summary}"
        );
    }

    #[test]
    fn a_broken_chain_is_reported_even_on_a_turn_that_ran_no_tool_call() {
        // A broken chain is broken whether or not the model happened to call a
        // tool, and reporting it early is what gives an operator a chance to
        // act before the next command runs unguarded.
        let dir = TempDir::new().unwrap();
        armed_codex_home(dir.path());
        let mut session = started_rollout_session(dir.path());
        std::fs::remove_dir_all(dir.path().join("guards")).unwrap();

        let events = session
            .normalize_progress_events(&json!({"type":"event_msg","payload":{"type":"task_complete"}}))
            .unwrap();
        assert!(is_silent_signal(&events), "got {events:?}");
    }

    #[test]
    fn a_reader_with_no_codex_home_makes_no_claim_about_arming() {
        // Fixtures and the stateless path have no run-private CODEX_HOME. They
        // must not manufacture a guardrail alarm out of that absence.
        let mut session = CodexRolloutProgressSession::new(None, None, None);
        session
            .normalize_progress_events(&json!({
                "type":"session_meta",
                "payload":{"id":"thread-guards","cwd":"/tmp/workspace"}
            }))
            .unwrap();
        let events = guard_trace_turn(&mut session, "call-1");
        assert!(!is_silent_signal(&events), "got {events:?}");
    }

    #[test]
    fn rollout_records_become_canonical_renderable_records() {
        let agent_echo = normalize_rollout(json!({
            "type":"event_msg",
            "payload":{"type":"agent_message","message":"answer"}
        }));
        assert_eq!(agent_echo, json!({"type":"system"}));

        let injected_user = normalize_rollout(json!({
            "type":"response_item",
            "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"context"}]}
        }));
        assert_eq!(injected_user, json!({"type":"system"}));

        let empty_assistant = normalize_rollout(json!({
            "type":"response_item",
            "payload":{"type":"message","role":"assistant","content":[]}
        }));
        assert_eq!(empty_assistant, json!({"type":"system"}));

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
        assert_eq!(output["type"], "tool_result");
        assert_eq!(output["tool_name"], "Bash");
        assert_eq!(output["tool_input"], json!({"command":"echo rollout"}));
        assert_eq!(output["content"], "rollout\n");
        assert_eq!(output["is_error"], false);
    }

    #[test]
    fn task_started_rearms_last_agent_message_fallback_for_each_turn() {
        let mut session = CodexTranscriptSession::default();

        session.normalize(json!({"type":"event_msg","payload":{"type":"task_started"}}));
        assert_eq!(
            session.normalize(json!({
                "type":"response_item",
                "payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"first"}]}
            }))["type"],
            "assistant"
        );
        let first_complete = session.normalize(json!({
            "type":"event_msg",
            "payload":{"type":"task_complete","last_agent_message":"first"}
        }));
        assert_eq!(first_complete["type"], "system");
        assert_eq!(first_complete["subtype"], "task_complete");

        session.normalize(json!({"type":"event_msg","payload":{"type":"task_started"}}));
        let second_complete = session.normalize(json!({
            "type":"event_msg",
            "payload":{"type":"task_complete","last_agent_message":"final"}
        }));
        assert_eq!(second_complete["type"], "assistant");
        assert_eq!(second_complete["content"][0]["text"], "final");
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
                stop_reason: StopReason::Interrupted,
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
    fn rollout_call_with_no_output_is_flagged_abandoned_at_task_complete() {
        let mut session = CodexRolloutProgressSession::new(None, None, None);
        session
            .normalize_progress_events(&json!({
                "type":"session_meta",
                "payload":{"id":"thread-abandon","cwd":"/tmp/workspace"}
            }))
            .unwrap();
        session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"function_call",
                    "name":"exec_command",
                    "call_id":"call-abandon",
                    "arguments":r#"{"cmd":"sleep 999"}"#
                }
            }))
            .unwrap();

        // No function_call_output for call-abandon ever arrives (probe 6) —
        // task_complete still fires.
        let events = session
            .normalize_progress_events(&json!({
                "type":"event_msg",
                "payload":{"type":"task_complete","turn_id":"turn-abandon"}
            }))
            .unwrap();

        assert_eq!(
            events,
            vec![
                WorkerEvent::Notification {
                    session_id: "thread-abandon".into(),
                    message: crate::codex::unobserved_command_notification("sleep 999"),
                },
                WorkerEvent::Stop {
                    session_id: "thread-abandon".into(),
                    stop_hook_active: false,
                    stop_reason: StopReason::Completed,
                },
            ]
        );
    }

    #[test]
    fn rollout_call_with_no_output_is_flagged_abandoned_at_turn_aborted() {
        let mut session = CodexRolloutProgressSession::new(None, None, None);
        session
            .normalize_progress_events(&json!({
                "type":"session_meta",
                "payload":{"id":"thread-abort","cwd":"/tmp/workspace"}
            }))
            .unwrap();
        session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call",
                    "name":"exec",
                    "call_id":"call-abort",
                    "input":"sleep 999"
                }
            }))
            .unwrap();

        let events = session
            .normalize_progress_events(&json!({
                "type":"event_msg",
                "payload":{"type":"turn_aborted","reason":"interrupted"}
            }))
            .unwrap();

        assert_eq!(
            events,
            vec![
                WorkerEvent::Notification {
                    session_id: "thread-abort".into(),
                    message: crate::codex::unobserved_command_notification("sleep 999"),
                },
                WorkerEvent::Notification {
                    session_id: "thread-abort".into(),
                    message: "turn aborted: interrupted".into(),
                },
                WorkerEvent::Stop {
                    session_id: "thread-abort".into(),
                    stop_hook_active: false,
                    stop_reason: StopReason::Interrupted,
                },
            ]
        );
    }

    #[test]
    fn rollout_pending_non_shell_call_is_not_flagged_abandoned() {
        let mut session = CodexRolloutProgressSession::new(None, None, None);
        session
            .normalize_progress_events(&json!({
                "type":"session_meta",
                "payload":{"id":"thread-patch","cwd":"/tmp/workspace"}
            }))
            .unwrap();
        session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{
                    "type":"custom_tool_call",
                    "name":"apply_patch",
                    "call_id":"call-patch",
                    "input":{"patch":"*** Begin Patch\n*** End Patch"}
                }
            }))
            .unwrap();

        // No matching call output ever arrives for call-patch, but it is not
        // exec/shell-shaped (no `command` field on its tool_input), so the
        // turn boundary must not flag it as an unobserved command.
        let events = session
            .normalize_progress_events(&json!({
                "type":"event_msg",
                "payload":{"type":"turn_aborted","reason":"interrupted"}
            }))
            .unwrap();

        assert_eq!(
            events,
            vec![
                WorkerEvent::Notification {
                    session_id: "thread-patch".into(),
                    message: "turn aborted: interrupted".into(),
                },
                WorkerEvent::Stop {
                    session_id: "thread-patch".into(),
                    stop_hook_active: false,
                    stop_reason: StopReason::Interrupted,
                },
            ]
        );
    }
}
