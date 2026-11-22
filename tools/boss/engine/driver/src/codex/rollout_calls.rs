//! Rollout tool-call correlation, including the JS cell harness's
//! yield → `wait` continuation.
//!
//! A rollout tool output carries only a `call_id`, so every consumer has to
//! hold the matching call to describe a result at all. Under the cell
//! harness (see [`boss_engine_codex_rollout::cell`]) that pairing is not
//! enough: a command that outlives its cell's yield window has its call
//! closed by a **placeholder** (`Script running with cell ID 1`), and its
//! real output arrives on a later `wait` call bound to nothing.
//!
//! Two things broke because of that, both from the same cause:
//!
//! - the record carrying the command's output — including the `cube pr
//!   create` URL the engine's primary PR-URL capture path reads — was
//!   attributed to a `wait` tool and dropped, while the record that *was*
//!   fed to capture held only the placeholder.
//! - the placeholder closed the call, so a still-running command looked
//!   observed and [`crate::codex::UNOBSERVED_COMMAND_MARKER`] detection
//!   could not see it.
//!
//! [`RolloutCallTracker`] closes both by using the **shell session**, not the
//! JavaScript cell, as the durable unit of correlation. A cell id bridges a
//! yielded call to its first poll, but the structured chunk's `session_id`
//! then binds every later polling cell to the originating command. The call
//! it completes is therefore the *originating* call, so downstream consumers
//! see the real command paired with the real output and never learn that a
//! `wait` was involved.
//!
//! "Terminal" here is about the **command**, not the cell. `Script
//! completed` says the JavaScript cell finished; the command's own terminal
//! signal is the `exit_code` on the chunk the cell forwarded. A completed
//! cell whose chunk carries none (probe 6's shape — `chunk_id` plus
//! `session_id`, partial output, command still ticking) therefore keeps its
//! call open, and re-arms the cell binding the poll consumed so a further
//! `wait` still resolves.
//!
//! Correlation failures are [`CorrelationError`]s, never a silent drop: a
//! `wait` naming a cell this session never yielded means the tracker has
//! lost the thread, and a consumer that returned "no result" there would
//! hide exactly the gap this module exists to expose. The progress
//! normaliser turns each into an operator-visible unobserved-command
//! notification rather than only a skipped line.

use std::collections::{HashMap, VecDeque};

use boss_engine_codex_rollout::cell::{
    CellOutcome, CellOutput, is_cell_wait_tool, is_write_stdin_tool, parse_cell_output, payload_is_running_chunk,
    payload_is_truncated_output, payload_reports_exit_code, payload_session_id, script_is_continuation_only,
    value_is_running_chunk, value_reports_exit_code, wait_target_cell_id, write_stdin_session_id,
};
use boss_engine_codex_rollout::{CanonicalRolloutToolOutput, canonical_rollout_tool_output, flatten_tool_output_text};
use serde_json::Value;

/// Bound on tracked entries per map for a tracker that is drained at every
/// turn boundary — the progress-side [`RolloutCallTracker`], whose open calls
/// are fully drained (see [`RolloutCallTracker::drain_open_calls`])
/// immediately before every `Stop`. Its occupancy therefore resets to zero on
/// every turn regardless of how long the surrounding Codex session runs, so
/// 256 concurrently-outstanding calls *within a single turn* is already a
/// pathological turn. Eviction here means Boss loses the ability to ever
/// report the call as abandoned, which is strictly worse than the
/// unobserved-command notification this tracking exists to feed — so it is
/// logged at `error!` in [`BoundedMap::insert`].
pub(super) const MAX_TRACKED_ROLLOUT_CALLS_PER_TURN: usize = 256;

/// Bound on tracked entries per map for the transcript-rendering correlator,
/// which the live-status loop (`live_status_loop.rs::run_slot_loop`)
/// constructs **once per worker slot and keeps for the life of the run**,
/// feeding it every tail entry across every turn. Unlike
/// [`MAX_TRACKED_ROLLOUT_CALLS_PER_TURN`] there is no per-turn drain: a
/// resolved call is removed as soon as its output arrives, so occupancy only
/// grows from calls that are *abandoned* (never observed completing) — the
/// same condition `codex_unobserved_command::MAX_COMMANDS_PER_EXECUTION` (50
/// distinct commands) bounds on the engine side, but with nothing draining
/// this tracker between turns. Sized generously above that 50-per-session
/// ceiling (each entry is a small [`RolloutToolCall`] — a tool name plus its
/// JSON input — so the worst case is a few MB, not a correctness concern) to
/// make eviction a true tail event rather than something an ordinary long
/// session runs into. Where it must still exist — no cap is infinite —
/// eviction is loud, because it silently orphans a call's eventual output
/// (rendered as unmatched even though the command genuinely completed and
/// Boss simply stopped remembering it started).
pub(super) const MAX_TRACKED_TRANSCRIPT_TOOL_CALLS: usize = 4096;

/// One canonicalised rollout tool call, held open until its result arrives.
///
/// Serialisable because an open call is part of what a readopted progress
/// session has to carry forward: dropping it turns the matching output record
/// into an unpaired one. See [`RolloutCallSnapshot`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct RolloutToolCall {
    pub(super) tool_name: String,
    pub(super) tool_input: Value,
    /// Whether this call itself starts a shell command, as opposed to being
    /// a pure continuation of one already running. See
    /// [`boss_engine_codex_rollout::CanonicalRolloutToolCall::issues_command`]
    /// for why this has to be carried from canonicalisation rather than
    /// re-derived from `tool_input`. Defaults to `true` (not a continuation)
    /// for a snapshot persisted before this field existed, which is the
    /// conservative reading — it only ever widens session-resolution
    /// eligibility for calls that never asserted otherwise.
    #[serde(default = "default_issues_command")]
    pub(super) issues_command: bool,
    /// When [`RolloutCallTracker::observe_output`] leaves a write_stdin poll
    /// open, the session id observed on the chunk (or [`OpenPoll::Unresolved`]
    /// when the poll used a dynamic session expression and the chunk named
    /// none). Drain reads this instead of re-parsing `tool_input`, which
    /// cannot recover a `load()`/`store()` session id.
    #[serde(default)]
    pub(super) open_poll: Option<OpenPoll>,
}

/// A write_stdin poll that stayed open at a turn boundary.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum OpenPoll {
    Session(String),
    Unresolved,
}

fn default_issues_command() -> bool {
    true
}

/// Render a display string for one abandoned rollout tool call, for the
/// unobserved-command notification. Prefers the reshaped `command` field
/// (present for the `exec`/shell-shaped calls this detection cares about);
/// falls back to `"<tool_name> <tool_input>"` for any other call shape so a
/// non-shell tool call is still described rather than silently dropped.
pub(super) fn rollout_call_command_display(call: &RolloutToolCall) -> String {
    match call.tool_input.get("command").and_then(Value::as_str) {
        Some(command) => command.to_owned(),
        None => format!("{} {}", call.tool_name, call.tool_input),
    }
}

/// The unobserved-command display for a call still open at a turn boundary,
/// or `None` when the call should not be flagged.
///
/// A call that issued a shell command is flagged by its command. A `write_stdin`
/// poll that never resolved to a tracked origin (the session was started in an
/// earlier turn, so the tracker had no binding) and is still open is itself the
/// only observation of that command, so it is flagged too — by the session it
/// polled when one is known, otherwise as an unresolved poll. The suffix is
/// "no terminal result observed" because the tracker cannot tell a still-running
/// chunk from an unconfirmed projection. Compute-only cells and non-shell
/// tools carry neither and are dropped.
pub(super) fn abandoned_call_display(call: &RolloutToolCall) -> Option<String> {
    if call.issues_command {
        return call
            .tool_input
            .get("command")
            .and_then(Value::as_str)
            .map(|_| rollout_call_command_display(call));
    }
    const NO_TERMINAL: &str = "no terminal result observed";
    let session_id = match &call.open_poll {
        Some(OpenPoll::Session(id)) => Some(id.as_str()),
        _ => None,
    }
    .map(str::to_owned)
    .or_else(|| write_stdin_session_id(&call.tool_input));
    if let Some(session_id) = session_id {
        return Some(format!("write_stdin session {session_id} ({NO_TERMINAL})"));
    }
    matches!(call.open_poll, Some(OpenPoll::Unresolved)).then(|| format!("write_stdin poll ({NO_TERMINAL})"))
}

/// What a consumer should do with a tool-call record.
#[derive(Debug)]
pub(super) enum CallDisposition {
    /// A new tool call. Announce it (`PreToolUse` / assistant `tool_use`).
    Announce,
    /// A `wait` poll of a cell whose command was already announced when the
    /// cell started. Nothing new happened; announcing it again would report
    /// a tool named `wait` as the worker's activity and double-count the
    /// originating command in every `PreToolUse` consumer.
    Continuation,
}

/// What a consumer should do with a tool-output record.
#[derive(Debug)]
pub(super) enum OutputDisposition {
    /// The originating call is finished. Report it with this record's own
    /// output — `call` is the call that issued the command, which for a
    /// continuation is *not* the call this output's `call_id` names.
    Completed(RolloutToolCall),
    /// The cell forwarded output, but omitted the command's terminal signal.
    /// Report this record so downstream consumers retain its body, while
    /// keeping the call open for abandoned-command detection at the boundary.
    UnconfirmedResult(RolloutToolCall),
    /// The command is still running: nothing about it is observed yet, and
    /// the call stays open. `cell_id` is the cell a further `wait` must poll
    /// to reach the command's real result, when the record named one — the
    /// yielded cell for a yield placeholder, or the cell just polled when a
    /// `Script completed` turned out to carry a chunk with no exit code.
    StillRunning { cell_id: Option<String> },
}

/// A correlation the tracker could not resolve. Surfaced, never swallowed.
#[derive(Debug)]
pub(super) enum CorrelationError {
    /// A `wait` call carried no `cell_id`, or named a cell this session
    /// never saw yield.
    UnresolvableWait(String),
    /// A tool output arrived for a call this session is not tracking.
    UnmatchedOutput(String),
}

impl CorrelationError {
    /// The string this failure contributes to an unobserved-command
    /// notification.
    ///
    /// A correlation failure is an unobserved command in the exact sense
    /// `boss_engine_core::codex_unobserved_command` means: Boss saw a
    /// command's record go past and could not attribute its outcome, so any
    /// completion claim resting on it is unconfirmed. There is no command
    /// string to quote — that is the whole problem — so the failure's own
    /// detail stands in its place.
    pub(super) fn lost_command_display(&self) -> String {
        match self {
            Self::UnresolvableWait(detail) => detail.clone(),
            Self::UnmatchedOutput(call_id) => {
                format!("rollout tool output {call_id} matched no tracked call")
            }
        }
    }
}

impl std::fmt::Display for CorrelationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnresolvableWait(detail) => {
                write!(f, "unresolvable rollout cell continuation: {detail}")
            }
            Self::UnmatchedOutput(call_id) => write!(f, "unmatched rollout tool output {call_id}"),
        }
    }
}

/// Insertion-ordered map with a hard capacity: inserting past `cap` evicts
/// the oldest entry. `purpose` names the owning tracker in the eviction log
/// so an operator can tell a per-turn eviction from a transcript one.
struct BoundedMap<V> {
    entries: HashMap<String, V>,
    order: VecDeque<String>,
    cap: usize,
    purpose: &'static str,
}

impl<V> BoundedMap<V> {
    fn new(cap: usize, purpose: &'static str) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            cap,
            purpose,
        }
    }

    /// Insert `key`, replacing any existing value in place (keeping its
    /// position) so a re-observed record does not age out its neighbours.
    fn insert(&mut self, key: String, value: V) {
        if let std::collections::hash_map::Entry::Occupied(mut existing) = self.entries.entry(key.clone()) {
            existing.insert(value);
            return;
        }
        while self.entries.len() >= self.cap {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
            tracing::error!(
                evicted_key = oldest,
                cap = self.cap,
                purpose = self.purpose,
                "codex rollout: evicted a tracked correlation entry before its completion was \
                 observed — this tracker holds more than its cap of outstanding entries; the \
                 evicted call can no longer be reported as abandoned and its eventual output \
                 will render as unmatched",
            );
        }
        self.entries.insert(key.clone(), value);
        self.order.push_back(key);
    }

    fn remove(&mut self, key: &str) -> Option<V> {
        let value = self.entries.remove(key)?;
        if let Some(index) = self.order.iter().position(|known| known == key) {
            self.order.remove(index);
        }
        Some(value)
    }

    fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    fn get(&self, key: &str) -> Option<&V> {
        self.entries.get(key)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        self.entries.get_mut(key)
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drain every value, oldest first, leaving the map empty.
    fn drain_in_order(&mut self) -> Vec<V> {
        let ordered = std::mem::take(&mut self.order);
        let drained = ordered
            .into_iter()
            .filter_map(|key| self.entries.remove(&key))
            .collect();
        // Defensive: `order` is expected to mirror `entries`' keys exactly,
        // but clear here too so a future divergence can never leak an entry
        // past the boundary that drained it.
        self.entries.clear();
        drained
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    /// Every entry, oldest first, without draining. The order is the eviction
    /// order, so a snapshot taken through this round-trips the LRU position of
    /// each entry rather than re-inventing it from `HashMap` iteration.
    fn entries_in_order(&self) -> Vec<(String, V)>
    where
        V: Clone,
    {
        self.order
            .iter()
            .filter_map(|key| self.entries.get(key).map(|value| (key.clone(), value.clone())))
            .collect()
    }

    /// Replace the map's contents with `entries`, oldest first.
    fn restore_in_order(&mut self, entries: Vec<(String, V)>) {
        self.clear();
        for (key, value) in entries {
            self.insert(key, value);
        }
    }
}

/// A tracker's correlation state, in a form a readopted session can be
/// re-seeded from.
///
/// All four maps are carried, not just the open calls: a cell or session binding dropped
/// across a restart would make the next `wait` name a cell the tracker never
/// saw yield, which [`RolloutCallTracker::observe_call`] reports as a
/// correlation failure — a real signal that would then be fired by the restart
/// rather than by a genuine loss of the thread.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct RolloutCallSnapshot {
    open_calls: Vec<(String, RolloutToolCall)>,
    cells: Vec<(String, String)>,
    #[serde(default)]
    sessions: Vec<(String, String)>,
    waits: Vec<(String, WaitBinding)>,
}

/// Per-reader correlation state. One tracker belongs to one rollout reader;
/// concurrent Codex runs never share one.
pub(super) struct RolloutCallTracker {
    /// Calls announced but not yet resolved. A yielded cell's originating
    /// call stays here — that is what keeps a still-running command visible
    /// to abandoned-command detection.
    calls: BoundedMap<RolloutToolCall>,
    /// Yielded cell id → originating call id. Removed when a `wait` binds
    /// the cell, so this never outgrows `calls`.
    cells: BoundedMap<String>,
    /// Live shell session id → originating call id. The session outlives the
    /// JavaScript cell that first observed it, so later polling cells resolve
    /// through this map instead of being treated as unrelated commands.
    sessions: BoundedMap<String>,
    /// `wait` call id → what that poll targets.
    waits: BoundedMap<WaitBinding>,
}

/// What a `wait` call polls: the call that issued the command, and the cell
/// id it named. The cell id is kept so a poll that comes back
/// `Script completed` with a still-running chunk can re-arm the binding it
/// consumed and let the next poll of the same cell resolve.
///
/// Serialisable for the same reason [`RolloutToolCall`] is: a readopted
/// session that lost its pending bindings would report a correlation failure
/// the run never actually had. See [`RolloutCallSnapshot`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct WaitBinding {
    origin: String,
    cell_id: String,
}

impl Default for RolloutCallTracker {
    /// A per-turn tracker (the progress-side default), capped at
    /// [`MAX_TRACKED_ROLLOUT_CALLS_PER_TURN`].
    fn default() -> Self {
        Self::with_capacity(MAX_TRACKED_ROLLOUT_CALLS_PER_TURN, "per-turn")
    }
}

impl RolloutCallTracker {
    /// A tracker with an explicit cap. Use
    /// [`MAX_TRACKED_TRANSCRIPT_TOOL_CALLS`] for the long-lived
    /// transcript-rendering correlator, which is never drained per turn.
    pub(super) fn with_capacity(cap: usize, purpose: &'static str) -> Self {
        Self {
            calls: BoundedMap::new(cap, purpose),
            cells: BoundedMap::new(cap, purpose),
            sessions: BoundedMap::new(cap, purpose),
            waits: BoundedMap::new(cap, purpose),
        }
    }

    /// Bind a canonicalised tool call.
    ///
    /// A `wait` resolves to the call whose cell it polls; anything else is a
    /// new call to announce.
    pub(super) fn observe_call(
        &mut self,
        call_id: &str,
        call: RolloutToolCall,
    ) -> Result<CallDisposition, CorrelationError> {
        if !is_cell_wait_tool(&call.tool_name) {
            let issues_command = call.issues_command;
            self.calls.insert(call_id.to_owned(), call);
            return if issues_command {
                Ok(CallDisposition::Announce)
            } else {
                // A pure continuation (`write_stdin` cell or function call)
                // does not start a command. Announcing it as Bash would put
                // the JavaScript poll in live status and, if the call stayed
                // open, drain it as an unobserved shell command.
                Ok(CallDisposition::Continuation)
            };
        }
        let cell_id = wait_target_cell_id(&call.tool_input).ok_or_else(|| {
            CorrelationError::UnresolvableWait(format!(
                "{} call {call_id} carries no cell_id in {}",
                call.tool_name, call.tool_input
            ))
        })?;
        let origin = self.cells.remove(&cell_id).ok_or_else(|| {
            CorrelationError::UnresolvableWait(format!(
                "{} call {call_id} polls cell {cell_id}, which this session never saw yield",
                call.tool_name
            ))
        })?;
        self.waits.insert(call_id.to_owned(), WaitBinding { origin, cell_id });
        Ok(CallDisposition::Continuation)
    }

    /// Resolve a tool-output record against the call that issued the command.
    ///
    /// `output` is the record's verbatim `payload.output` value.
    ///
    /// A record is terminal for the **command** only when the cell harness
    /// says the command finished, which the `Script completed` header does
    /// not: it describes the JavaScript cell. Two shapes therefore keep the
    /// call open — the yield placeholder, and a completed cell whose
    /// forwarded chunk carries no exit code
    /// ([`payload_is_running_chunk`], probe 6's shape).
    pub(super) fn observe_output(
        &mut self,
        call_id: &str,
        output: &Value,
    ) -> Result<OutputDisposition, CorrelationError> {
        let parsed = parse_cell_output(&flatten_tool_output_text(output));
        let session_id = parsed.as_ref().and_then(|parsed| payload_session_id(&parsed.payload));
        let binding = self.waits.remove(call_id);
        // A cell that starts a new command owns its result even when the
        // shell happens to reuse a previously tracked session. Only a pure
        // continuation cell (`tools.write_stdin`) resolves through sessions.
        // `issues_command` is carried from canonicalisation, not re-derived
        // from `tool_input.command`: that field is the already-extracted
        // command string for a command-bearing cell, not its raw script, so
        // re-running `commands_from_cell_script` on it here would find
        // nothing and misclassify every command-bearing cell too.
        let is_continuation_cell = self.calls.get(call_id).is_some_and(|call| !call.issues_command);
        let call_session_id = self
            .calls
            .get(call_id)
            .and_then(|call| write_stdin_session_id(&call.tool_input));
        let origin = is_continuation_cell
            .then(|| {
                session_id
                    .clone()
                    .or_else(|| call_session_id.clone())
                    .and_then(|session_id| self.sessions.get(&session_id).cloned())
            })
            .flatten()
            .or_else(|| binding.as_ref().map(|binding| binding.origin.clone()))
            .unwrap_or_else(|| call_id.to_owned());
        if !self.calls.contains_key(&origin) {
            return Err(CorrelationError::UnmatchedOutput(call_id.to_owned()));
        }
        match parsed {
            // The cell yielded under a (possibly new) cell id: bind that id
            // so the next `wait` resolves back to this same command.
            Some(CellOutput {
                outcome: CellOutcome::Yielded { cell_id },
                ..
            }) => {
                self.cells.insert(cell_id.clone(), origin.clone());
                self.remember_open_poll(&origin, call_id, session_id.as_deref().or(call_session_id.as_deref()));
                if origin != call_id {
                    self.calls.remove(call_id);
                }
                return Ok(OutputDisposition::StillRunning { cell_id: Some(cell_id) });
            }
            // The cell finished but the command did not. Re-arm the cell
            // binding this poll consumed, so polling the same cell again
            // still reaches this command; when the model stops polling
            // instead, the call stays open and drains as abandoned — which
            // is exactly the reported failure probe 6 reproduced.
            Some(CellOutput {
                outcome: CellOutcome::Completed,
                payload,
            }) if payload_is_running_chunk(&payload) => {
                if let Some(session_id) = payload_session_id(&payload) {
                    self.sessions.insert(session_id, origin.clone());
                }
                let cell_id = binding.map(|binding| binding.cell_id);
                if let Some(cell_id) = cell_id.clone() {
                    self.cells.insert(cell_id, origin.clone());
                }
                self.remember_open_poll(
                    &origin,
                    call_id,
                    payload_session_id(&payload).as_deref().or(call_session_id.as_deref()),
                );
                if origin != call_id {
                    self.calls.remove(call_id);
                }
                return Ok(OutputDisposition::StillRunning { cell_id });
            }
            Some(CellOutput {
                outcome: CellOutcome::Completed,
                payload,
            }) if payload_reports_exit_code(&payload) || payload_is_truncated_output(&payload) => {}
            // `text(r.output)` discards both `session_id` and `exit_code`.
            // A completed cell carrying that projection is not evidence that
            // the command terminated, so leave it open for the turn boundary.
            Some(CellOutput {
                outcome: CellOutcome::Completed,
                ..
            }) => {
                let call = self
                    .calls
                    .get(&origin)
                    .expect("origin call presence was just checked")
                    .clone();
                self.remember_open_poll(&origin, call_id, session_id.as_deref().or(call_session_id.as_deref()));
                if origin != call_id {
                    self.calls.remove(call_id);
                }
                return Ok(OutputDisposition::UnconfirmedResult(call));
            }
            _ => {
                // A continuation resolved to a different origin via its
                // session id even when this output is not a cell envelope.
                // Completing that origin requires positive terminal evidence;
                // a bare running chunk, unified-exec "still running" prose,
                // or a poll error must leave it open. The same check applies
                // when a non-command write_stdin poll resolves to itself
                // (sessions were drained at an earlier turn boundary):
                // "Process running" is not a terminal result.
                let self_resolved_write_stdin = origin == call_id
                    && self
                        .calls
                        .get(call_id)
                        .is_some_and(|call| !call.issues_command && is_write_stdin_poll(call));
                if origin != call_id || self_resolved_write_stdin {
                    match command_output_kind(output) {
                        CommandOutputKind::Terminal => {}
                        CommandOutputKind::Running => {
                            if origin != call_id {
                                self.calls.remove(call_id);
                            } else {
                                self.remember_open_poll(
                                    &origin,
                                    call_id,
                                    session_id.as_deref().or(call_session_id.as_deref()),
                                );
                            }
                            return Ok(OutputDisposition::StillRunning { cell_id: None });
                        }
                        CommandOutputKind::Unconfirmed => {
                            if origin != call_id {
                                self.calls.remove(call_id);
                            } else {
                                self.remember_open_poll(
                                    &origin,
                                    call_id,
                                    session_id.as_deref().or(call_session_id.as_deref()),
                                );
                            }
                            let call = self
                                .calls
                                .get(&origin)
                                .expect("origin call presence was just checked")
                                .clone();
                            return Ok(OutputDisposition::UnconfirmedResult(call));
                        }
                    }
                }
            }
        }
        let bound_session = session_id.or(call_session_id);
        let call = self
            .calls
            .remove(&origin)
            .expect("origin call presence was just checked");
        if let Some(session_id) = bound_session {
            self.sessions.remove(&session_id);
        }
        if origin != call_id {
            self.calls.remove(call_id);
        }
        Ok(OutputDisposition::Completed(call))
    }

    /// Persist the observed session (or an unresolved-poll marker) on a
    /// self-resolved write_stdin continuation left open, and bind the session
    /// to that open poll so a subsequent poll of the same session closes it.
    fn remember_open_poll(&mut self, origin: &str, call_id: &str, session_id: Option<&str>) {
        if origin != call_id {
            return;
        }
        let Some(call) = self.calls.get_mut(origin) else {
            return;
        };
        if call.issues_command || !is_write_stdin_poll(call) {
            return;
        }
        let open_poll = match session_id {
            Some(id) if !id.is_empty() => {
                self.sessions.insert(id.to_owned(), origin.to_owned());
                OpenPoll::Session(id.to_owned())
            }
            _ => OpenPoll::Unresolved,
        };
        call.open_poll = Some(open_poll);
    }

    /// Whether any call is still open.
    pub(super) fn has_open_calls(&self) -> bool {
        !self.calls.is_empty()
    }

    /// Drain every still-open call, oldest first, and forget all pending
    /// cell / continuation bindings. Called at a turn boundary: a call left
    /// open there is an abandoned command, and its cell bindings cannot
    /// legitimately be resolved by a later, unrelated turn.
    pub(super) fn drain_open_calls(&mut self) -> Vec<RolloutToolCall> {
        self.cells.clear();
        self.sessions.clear();
        self.waits.clear();
        self.calls.drain_in_order()
    }

    /// Capture this tracker's correlation state for a later readoption.
    pub(super) fn snapshot(&self) -> RolloutCallSnapshot {
        RolloutCallSnapshot {
            open_calls: self.calls.entries_in_order(),
            cells: self.cells.entries_in_order(),
            sessions: self.sessions.entries_in_order(),
            waits: self.waits.entries_in_order(),
        }
    }

    /// Re-seed this tracker from a snapshot, replacing whatever it held.
    pub(super) fn restore(&mut self, snapshot: RolloutCallSnapshot) {
        self.calls.restore_in_order(snapshot.open_calls);
        self.cells.restore_in_order(snapshot.cells);
        self.sessions.restore_in_order(snapshot.sessions);
        self.waits.restore_in_order(snapshot.waits);
    }

    /// Forget everything — a new session owns none of the previous one's
    /// correlation state.
    pub(super) fn reset(&mut self) {
        self.calls.clear();
        self.cells.clear();
        self.sessions.clear();
        self.waits.clear();
    }
}

/// Command-status evidence on a non-cell tool output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandOutputKind {
    Terminal,
    Running,
    Unconfirmed,
}

/// Shared normaliser for direct (non-cell-envelope) poll results.
///
/// Inspect the raw JSON value first: flattening a chunk object keeps only a
/// `text` field and would drop `exit_code` / `chunk_id`. Unified-exec prose
/// is then read from the header *before* the first `Output:` line, so a
/// still-running header is never overridden by an exit-looking stdout line.
fn command_output_kind(output: &Value) -> CommandOutputKind {
    if value_reports_exit_code(output) {
        return CommandOutputKind::Terminal;
    }
    if value_is_running_chunk(output) {
        return CommandOutputKind::Running;
    }
    let text = flatten_tool_output_text(output);
    if payload_reports_exit_code(&text) {
        return CommandOutputKind::Terminal;
    }
    if payload_is_running_chunk(&text) {
        return CommandOutputKind::Running;
    }
    let header = unified_exec_header(&text);
    if prose_reports_process_running(header) {
        return CommandOutputKind::Running;
    }
    if prose_reports_process_exit(header) || payload_is_truncated_output(&text) {
        return CommandOutputKind::Terminal;
    }
    CommandOutputKind::Unconfirmed
}

fn is_write_stdin_poll(call: &RolloutToolCall) -> bool {
    is_write_stdin_tool(&call.tool_name)
        || write_stdin_session_id(&call.tool_input).is_some()
        || call
            .tool_input
            .get("script")
            .and_then(Value::as_str)
            .is_some_and(script_is_continuation_only)
}

fn unified_exec_header(text: &str) -> &str {
    text.split_once("\nOutput:").map(|(header, _)| header).unwrap_or(text)
}

fn prose_reports_process_exit(text: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix("Process exited with code ")
            .is_some_and(|rest| rest.chars().next().is_some_and(|ch| ch == '-' || ch.is_ascii_digit()))
    })
}

fn prose_reports_process_running(text: &str) -> bool {
    text.contains("Process running with session ID")
}

/// Parse a tool output's body and error flag, unwrapping the cell-harness
/// envelope first when there is one.
///
/// The harness wraps the command's real result in a prose header
/// (`Script completed` / `Wall time N seconds` / `Output:`). That header
/// makes the payload unparseable as the structured chunk it is, so the
/// chunk's own top-level `exit_code` — the only positive "this command
/// failed" signal the rollout dialect carries — is invisible until the
/// envelope is peeled. Non-cell outputs are parsed exactly as before.
pub(super) fn cell_aware_tool_output(output: &Value) -> CanonicalRolloutToolOutput {
    match parse_cell_output(&flatten_tool_output_text(output)) {
        Some(parsed) => canonical_rollout_tool_output(&Value::String(parsed.payload)),
        None => canonical_rollout_tool_output(output),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(command: &str) -> RolloutToolCall {
        RolloutToolCall {
            tool_name: "Bash".to_owned(),
            tool_input: json!({"command": command}),
            issues_command: true,
            open_poll: None,
        }
    }

    fn wait(cell_id: &str) -> RolloutToolCall {
        RolloutToolCall {
            tool_name: "wait".to_owned(),
            tool_input: json!({"cell_id": cell_id, "yield_time_ms": 30000}),
            issues_command: true,
            open_poll: None,
        }
    }

    /// Canonicalise a raw cell-harness `exec` script exactly as production
    /// does, so tests exercise the real `tool_input`/`issues_command` shape
    /// (the extracted command, never the script) rather than a hand-built
    /// approximation.
    fn cell_exec(call_id: &str, script: &str) -> RolloutToolCall {
        cell_exec_named("exec", call_id, script)
    }

    fn cell_exec_named(name: &str, call_id: &str, script: &str) -> RolloutToolCall {
        let payload = json!({"name": name, "call_id": call_id, "input": script})
            .as_object()
            .unwrap()
            .clone();
        let canonical = boss_engine_codex_rollout::canonical_rollout_tool_call("custom_tool_call", &payload)
            .expect("canonicalises");
        RolloutToolCall {
            tool_name: canonical.tool_name,
            tool_input: canonical.tool_input,
            issues_command: canonical.issues_command,
            open_poll: None,
        }
    }

    fn write_stdin_call(call_id: &str, session_id: u32) -> RolloutToolCall {
        let payload = json!({
            "name": "write_stdin",
            "call_id": call_id,
            "arguments": format!(r#"{{"session_id":{session_id},"chars":""}}"#)
        })
        .as_object()
        .unwrap()
        .clone();
        let canonical =
            boss_engine_codex_rollout::canonical_rollout_tool_call("function_call", &payload).expect("canonicalises");
        RolloutToolCall {
            tool_name: canonical.tool_name,
            tool_input: canonical.tool_input,
            issues_command: canonical.issues_command,
            open_poll: None,
        }
    }

    fn yield_placeholder(cell_id: u32) -> Value {
        json!(format!(
            "Script running with cell ID {cell_id}\nWall time 11.1 seconds\nOutput:\n"
        ))
    }

    fn completed(payload: &str) -> Value {
        json!([
            {"type":"input_text","text":"Script completed\nWall time 17.7 seconds\nOutput:\n"},
            {"type":"input_text","text":payload},
        ])
    }

    /// Probe 6's `wait` output verbatim: the cell completed, the chunk
    /// carries `session_id` and no `exit_code`, and only 8 of the command's
    /// 12 ticks arrived.
    const P6_RUNNING_CHUNK: &str = concat!(
        r#"{"chunk_id":"d0540d","wall_time_seconds":30.001035083,"session_id":8467,"#,
        r#""original_token_count":14,"output":"tick-1\ntick-2\ntick-3\ntick-4\ntick-5\ntick-6\ntick-7\ntick-8\n"}"#
    );

    #[test]
    fn wait_continuation_completes_the_originating_call() {
        let mut tracker = RolloutCallTracker::default();
        assert!(matches!(
            tracker.observe_call("A", bash("cube pr create --branch b")),
            Ok(CallDisposition::Announce)
        ));
        assert!(matches!(
            tracker.observe_output("A", &yield_placeholder(1)),
            Ok(OutputDisposition::StillRunning { .. })
        ));
        assert!(
            tracker.has_open_calls(),
            "a yielded command is still running, so its call must stay open"
        );
        assert!(matches!(
            tracker.observe_call("W", wait("1")),
            Ok(CallDisposition::Continuation)
        ));

        let payload = r#"{"chunk_id":"ab","exit_code":0,"output":"https://github.com/o/r/pull/9\n"}"#;
        let Ok(OutputDisposition::Completed(call)) = tracker.observe_output("W", &completed(payload)) else {
            panic!("the wait continuation must complete the originating call");
        };
        assert_eq!(
            call.tool_input.get("command").and_then(Value::as_str),
            Some("cube pr create --branch b"),
            "the completion must carry the command, not the wait's cell_id"
        );
        assert!(!tracker.has_open_calls());
    }

    #[test]
    fn repeated_yields_keep_the_call_open_across_successive_waits() {
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("slow build")).unwrap();
        tracker.observe_output("A", &yield_placeholder(1)).unwrap();
        tracker.observe_call("W1", wait("1")).unwrap();
        // The poll timed out too: the harness re-yields under a new cell id.
        assert!(matches!(
            tracker.observe_output("W1", &yield_placeholder(2)),
            Ok(OutputDisposition::StillRunning { .. })
        ));
        assert!(tracker.has_open_calls());
        tracker.observe_call("W2", wait("2")).unwrap();
        let Ok(OutputDisposition::Completed(call)) =
            tracker.observe_output("W2", &completed(r#"{"chunk_id":"done","exit_code":0,"output":"ok"}"#))
        else {
            panic!("the second continuation must complete the originating call");
        };
        assert_eq!(
            call.tool_input.get("command").and_then(Value::as_str),
            Some("slow build")
        );
    }

    #[test]
    fn a_completed_cell_whose_chunk_has_no_exit_code_keeps_the_command_open() {
        // Probe 6 end to end. `Script completed` describes the cell; the
        // command was still ticking, and the model stopped there. The call
        // must stay open so the turn boundary flags it.
        let mut tracker = RolloutCallTracker::default();
        tracker
            .observe_call("A", bash("sh -c 'for i in $(seq 1 12); ...'"))
            .unwrap();
        tracker.observe_output("A", &yield_placeholder(1)).unwrap();
        tracker.observe_call("W", wait("1")).unwrap();
        assert!(
            matches!(
                tracker.observe_output("W", &completed(P6_RUNNING_CHUNK)),
                Ok(OutputDisposition::StillRunning { cell_id: Some(ref id) }) if id == "1"
            ),
            "a chunk with no exit code must not complete the command"
        );
        assert!(tracker.has_open_calls());
        let abandoned = tracker.drain_open_calls();
        assert_eq!(abandoned.len(), 1);
        assert_eq!(
            rollout_call_command_display(&abandoned[0]),
            "sh -c 'for i in $(seq 1 12); ...'"
        );
    }

    #[test]
    fn re_polling_the_same_cell_after_a_running_chunk_still_resolves() {
        // Probe 6's shape, but the model keeps polling: the binding the
        // first `wait` consumed is re-armed, so the second poll reaches the
        // same command and its exit code closes it.
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("slow build")).unwrap();
        tracker.observe_output("A", &yield_placeholder(1)).unwrap();
        tracker.observe_call("W1", wait("1")).unwrap();
        tracker.observe_output("W1", &completed(P6_RUNNING_CHUNK)).unwrap();
        assert!(matches!(
            tracker.observe_call("W2", wait("1")),
            Ok(CallDisposition::Continuation)
        ));
        let Ok(OutputDisposition::Completed(call)) = tracker.observe_output(
            "W2",
            &completed(r#"{"chunk_id":"ab","exit_code":4,"output":"tick-12\n"}"#),
        ) else {
            panic!("a chunk carrying an exit code is terminal for the command");
        };
        assert_eq!(
            call.tool_input.get("command").and_then(Value::as_str),
            Some("slow build")
        );
        assert!(!tracker.has_open_calls());
    }

    #[test]
    fn an_unstructured_completed_cell_preserves_its_result_and_stays_open() {
        // A cell using `text(r.output)` loses both the session handle and
        // exit code. Boss cannot treat that projection as a pass, even when
        // the string happens to look like an exit code.
        let payload = "7";
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("echo hi")).unwrap();
        assert!(
            matches!(
                tracker.observe_output("A", &completed(payload)),
                Ok(OutputDisposition::UnconfirmedResult(_))
            ),
            "payload {payload:?} must remain unobserved"
        );
        assert_eq!(tracker.drain_open_calls().len(), 1);
    }

    #[test]
    fn a_truncated_completed_cell_with_exit_code_evidence_is_terminal() {
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("echo hi")).unwrap();
        assert!(matches!(
            tracker.observe_output(
                "A",
                &completed(
                    "Warning: truncated output (original token count: 11827)\n\ntouch: x: denied\n\"exit_code\":1\n"
                )
            ),
            Ok(OutputDisposition::Completed(_))
        ));
        assert!(tracker.drain_open_calls().is_empty());
    }

    #[test]
    fn a_truncated_still_running_chunk_with_no_exit_code_evidence_stays_open() {
        // Outer truncation applies to the tool-output block regardless of
        // whether the forwarded chunk reports an exit code, so a poll that
        // is still running but whose partial output was large enough to be
        // truncated must not close its call.
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("slow build")).unwrap();
        assert!(matches!(
            tracker.observe_output(
                "A",
                &completed("Warning: truncated output (original token count: 11827)\n\n{\"chunk_id\":\"5ce387\"}")
            ),
            Ok(OutputDisposition::UnconfirmedResult(_))
        ));
        assert_eq!(tracker.drain_open_calls().len(), 1);
    }

    #[test]
    fn multi_cell_session_is_observed_when_a_later_poll_reports_an_exit_code() {
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("slow build")).unwrap();
        tracker.observe_output("A", &completed(P6_RUNNING_CHUNK)).unwrap();

        // Each poll runs in a fresh cell. Its call id is unrelated to A, but
        // its structured chunk names A's shell session.
        tracker
            .observe_call(
                "poll-1",
                cell_exec(
                    "poll-1",
                    "const r = await tools.write_stdin({\"session_id\":8467,\"chars\":\"\"});",
                ),
            )
            .unwrap();
        tracker.observe_output("poll-1", &completed(P6_RUNNING_CHUNK)).unwrap();
        tracker
            .observe_call(
                "poll-2",
                cell_exec(
                    "poll-2",
                    "const r = await tools.write_stdin({\"session_id\":8467,\"chars\":\"\"});",
                ),
            )
            .unwrap();
        let result = tracker.observe_output(
            "poll-2",
            &completed(r#"{"chunk_id":"d0540d","session_id":8467,"exit_code":0,"output":"done\n"}"#),
        );
        assert!(
            matches!(result, Ok(OutputDisposition::Completed(call)) if rollout_call_command_display(&call) == "slow build")
        );
        assert!(tracker.drain_open_calls().is_empty());
    }

    #[test]
    fn session_without_an_exit_code_drains_as_abandoned() {
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("slow build")).unwrap();
        tracker.observe_output("A", &completed(P6_RUNNING_CHUNK)).unwrap();
        let abandoned = tracker.drain_open_calls();
        assert_eq!(
            abandoned.iter().map(rollout_call_command_display).collect::<Vec<_>>(),
            ["slow build"]
        );
    }

    #[test]
    fn one_shot_cell_without_session_or_exit_code_drains_as_abandoned() {
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("slow build")).unwrap();
        assert!(matches!(
            tracker.observe_output("A", &completed("partial output\n")),
            Ok(OutputDisposition::UnconfirmedResult(_))
        ));
        assert_eq!(
            tracker
                .drain_open_calls()
                .iter()
                .map(rollout_call_command_display)
                .collect::<Vec<_>>(),
            ["slow build"]
        );
    }

    #[test]
    fn a_command_cell_reusing_a_session_keeps_its_own_call_open() {
        // Both calls go through the real canonicaliser, so `tool_input` here is
        // the *extracted* command ("first" / "second"), never the raw script.
        // `issues_command` must therefore be derived from the raw script at
        // canonicalisation time, not from `tool_input` — that is the shape
        // this test pins.
        let mut tracker = RolloutCallTracker::default();
        tracker
            .observe_call(
                "A",
                cell_exec("A", "const r = await tools.exec_command({\"cmd\":\"first\"});"),
            )
            .unwrap();
        tracker.observe_output("A", &completed(P6_RUNNING_CHUNK)).unwrap();
        tracker
            .observe_call(
                "B",
                cell_exec("B", "const r = await tools.exec_command({\"cmd\":\"second\"});"),
            )
            .unwrap();
        assert!(matches!(
            tracker.observe_output("B", &completed(P6_RUNNING_CHUNK)),
            Ok(OutputDisposition::StillRunning { .. })
        ));
        let Ok(OutputDisposition::Completed(call)) = tracker.observe_output(
            "B",
            &completed(r#"{"chunk_id":"d0540d","session_id":8467,"exit_code":0,"output":"done\n"}"#),
        ) else {
            panic!("the second command must complete itself");
        };
        assert_eq!(rollout_call_command_display(&call), "second");
        assert_eq!(
            tracker
                .drain_open_calls()
                .iter()
                .map(rollout_call_command_display)
                .collect::<Vec<_>>(),
            ["first"]
        );
    }

    #[test]
    fn a_yielded_command_that_is_never_polled_drains_as_abandoned() {
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("bazel test //...")).unwrap();
        tracker.observe_output("A", &yield_placeholder(1)).unwrap();
        let abandoned = tracker.drain_open_calls();
        assert_eq!(abandoned.len(), 1);
        assert_eq!(rollout_call_command_display(&abandoned[0]), "bazel test //...");
        assert!(!tracker.has_open_calls());
    }

    #[test]
    fn a_wait_naming_an_unknown_cell_fails_loudly() {
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("echo hi")).unwrap();
        let err = tracker.observe_call("W", wait("7")).expect_err("must not resolve");
        assert!(
            matches!(&err, CorrelationError::UnresolvableWait(detail) if detail.contains("cell 7")),
            "got {err:?}"
        );
    }

    #[test]
    fn a_wait_without_a_cell_id_fails_loudly() {
        let mut tracker = RolloutCallTracker::default();
        let err = tracker
            .observe_call(
                "W",
                RolloutToolCall {
                    tool_name: "wait".to_owned(),
                    tool_input: json!({"yield_time_ms": 30000}),
                    issues_command: true,
                    open_poll: None,
                },
            )
            .expect_err("must not resolve");
        assert!(
            matches!(&err, CorrelationError::UnresolvableWait(detail) if detail.contains("no cell_id")),
            "got {err:?}"
        );
    }

    #[test]
    fn an_output_for_an_untracked_call_fails_loudly() {
        let mut tracker = RolloutCallTracker::default();
        let err = tracker
            .observe_output("ghost", &json!("hi"))
            .expect_err("must not resolve");
        assert!(
            matches!(&err, CorrelationError::UnmatchedOutput(id) if id == "ghost"),
            "got {err:?}"
        );
    }

    #[test]
    fn a_turn_boundary_forgets_pending_cell_bindings() {
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("echo hi")).unwrap();
        tracker.observe_output("A", &yield_placeholder(1)).unwrap();
        assert_eq!(tracker.drain_open_calls().len(), 1);
        // A later, unrelated turn must not be able to resolve cell 1.
        assert!(tracker.observe_call("W", wait("1")).is_err());
    }

    #[test]
    fn a_non_cell_output_completes_its_call_directly() {
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("echo hi")).unwrap();
        assert!(matches!(
            tracker.observe_output("A", &json!("hi\n")),
            Ok(OutputDisposition::Completed(_))
        ));
    }

    #[test]
    fn reset_forgets_every_correlation() {
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("A", bash("echo hi")).unwrap();
        tracker.observe_output("A", &yield_placeholder(1)).unwrap();
        tracker.reset();
        assert!(!tracker.has_open_calls());
        assert!(tracker.observe_call("W", wait("1")).is_err());
        assert!(tracker.observe_output("A", &json!("hi")).is_err());
    }

    #[test]
    fn tracked_calls_are_bounded() {
        let mut tracker = RolloutCallTracker::default();
        for index in 0..MAX_TRACKED_ROLLOUT_CALLS_PER_TURN + 10 {
            tracker.observe_call(&format!("call-{index}"), bash("echo hi")).unwrap();
        }
        assert_eq!(tracker.drain_open_calls().len(), MAX_TRACKED_ROLLOUT_CALLS_PER_TURN);
    }

    #[test]
    fn cell_envelope_is_peeled_before_the_structured_chunk_is_parsed() {
        // Probe 1's shape: the chunk's top-level `exit_code` is only
        // reachable once the harness's prose header is stripped.
        let parsed = cell_aware_tool_output(&completed(
            r#"{"chunk_id":"66d4c6","exit_code":7,"output":"LINE-ONE\nLINE-TWO\n"}"#,
        ));
        assert_eq!(parsed.body, "LINE-ONE\nLINE-TWO\n");
        assert!(parsed.is_error, "exit 7 must not read as a clean command");
    }

    #[test]
    fn cell_envelope_with_a_zero_exit_code_stays_clean() {
        let parsed = cell_aware_tool_output(&completed(r#"{"chunk_id":"ab","exit_code":0,"output":"ok\n"}"#));
        assert_eq!(parsed.body, "ok\n");
        assert!(!parsed.is_error);
    }

    #[test]
    fn a_cell_payload_that_is_not_structured_keeps_its_text() {
        // Probe 3: outer truncation prepends a warning, so the payload no
        // longer parses. The text still has to survive for the marker scan.
        let parsed = cell_aware_tool_output(&completed(
            "Warning: truncated output (original token count: 11827)\n\ntouch: x: Operation not permitted\n",
        ));
        assert!(parsed.body.contains("Operation not permitted"));
        assert!(
            !parsed.is_error,
            "an unobserved exit code is never promoted to an error"
        );
    }

    #[test]
    fn non_cell_outputs_parse_exactly_as_before() {
        let parsed = cell_aware_tool_output(&json!(r#"{"output":"boom\n","metadata":{"exit_code":1}}"#));
        assert_eq!(parsed.body, "boom\n");
        assert!(parsed.is_error);
    }

    /// The reported failure's wrapper shape: a `functions.exec` cell whose
    /// JavaScript awaits `exec_command` then `write_stdin`, and forwards the
    /// structured result. Completing with an `exit_code` must close the call.
    const WRAPPER_SCRIPT: &str = concat!(
        r#"let r = await tools.exec_command({cmd: "bazel test //tools/boss/engine/codex-rollout:codex-rollout_test", yield_time_ms: 30000});"#,
        r#"while (!("exit_code" in r)) { r = await tools.write_stdin({session_id: r.session_id, chars: "", yield_time_ms: 300000}); }"#,
        "text(JSON.stringify(r));",
    );

    #[test]
    fn a_completed_functions_exec_wrapper_with_exit_code_is_observed() {
        let mut tracker = RolloutCallTracker::default();
        assert!(matches!(
            tracker.observe_call("wrap", cell_exec_named("functions.exec", "wrap", WRAPPER_SCRIPT),),
            Ok(CallDisposition::Announce)
        ));
        let Ok(OutputDisposition::Completed(call)) = tracker.observe_output(
            "wrap",
            &completed(r#"{"chunk_id":"done","session_id":8467,"exit_code":0,"output":"2 tests passed\n"}"#),
        ) else {
            panic!("a wrapper that forwarded exit_code must complete");
        };
        assert_eq!(
            rollout_call_command_display(&call),
            "bazel test //tools/boss/engine/codex-rollout:codex-rollout_test"
        );
        assert!(tracker.drain_open_calls().is_empty());
    }

    #[test]
    fn a_write_stdin_function_call_completes_the_originating_wrapper() {
        let mut tracker = RolloutCallTracker::default();
        tracker
            .observe_call("wrap", cell_exec_named("functions.exec", "wrap", WRAPPER_SCRIPT))
            .unwrap();
        tracker.observe_output("wrap", &completed(P6_RUNNING_CHUNK)).unwrap();
        assert!(matches!(
            tracker.observe_call("poll", write_stdin_call("poll", 8467)),
            Ok(CallDisposition::Continuation)
        ));
        let Ok(OutputDisposition::Completed(call)) = tracker.observe_output(
            "poll",
            &completed(r#"{"chunk_id":"done","session_id":8467,"exit_code":0,"output":"done\n"}"#),
        ) else {
            panic!("write_stdin with exit_code must complete the originating command");
        };
        assert_eq!(
            rollout_call_command_display(&call),
            "bazel test //tools/boss/engine/codex-rollout:codex-rollout_test"
        );
        assert!(tracker.drain_open_calls().is_empty());
    }

    fn open_wrapper_then_write_stdin_poll() -> RolloutCallTracker {
        let mut tracker = RolloutCallTracker::default();
        tracker
            .observe_call("wrap", cell_exec_named("functions.exec", "wrap", WRAPPER_SCRIPT))
            .unwrap();
        tracker.observe_output("wrap", &completed(P6_RUNNING_CHUNK)).unwrap();
        tracker.observe_call("poll", write_stdin_call("poll", 8467)).unwrap();
        tracker
    }

    #[test]
    fn a_direct_write_stdin_without_terminal_evidence_keeps_the_origin_open() {
        let wrapper = "bazel test //tools/boss/engine/codex-rollout:codex-rollout_test";
        for (output, still_running) in [
            (json!(P6_RUNNING_CHUNK), true),
            (json!({"chunk_id":"d0540d","session_id":8467,"output":"tick"}), true),
            (json!("Process running with session ID 8467"), true),
            (json!("Error: failed to poll session 8467"), false),
        ] {
            let mut tracker = open_wrapper_then_write_stdin_poll();
            let result = tracker.observe_output("poll", &output);
            if still_running {
                assert!(
                    matches!(result, Ok(OutputDisposition::StillRunning { .. })),
                    "{result:?}"
                );
            } else {
                assert!(
                    matches!(result, Ok(OutputDisposition::UnconfirmedResult(_))),
                    "{result:?}"
                );
            }
            assert_eq!(
                tracker
                    .drain_open_calls()
                    .iter()
                    .map(rollout_call_command_display)
                    .collect::<Vec<_>>(),
                [wrapper]
            );
        }
    }

    #[test]
    fn a_direct_write_stdin_with_terminal_evidence_completes_the_origin() {
        for output in [
            json!(r#"{"chunk_id":"done","session_id":8467,"exit_code":0,"output":"done\n"}"#),
            json!({"chunk_id":"done","session_id":8467,"exit_code":0,"output":"done"}),
            json!("Process exited with code 0\nOutput:\ndone\n"),
        ] {
            let mut tracker = open_wrapper_then_write_stdin_poll();
            let Ok(OutputDisposition::Completed(call)) = tracker.observe_output("poll", &output) else {
                panic!("terminal evidence must complete the originating command: {output}");
            };
            assert_eq!(
                rollout_call_command_display(&call),
                "bazel test //tools/boss/engine/codex-rollout:codex-rollout_test"
            );
            assert!(tracker.drain_open_calls().is_empty());
        }
    }

    #[test]
    fn a_running_header_is_not_overridden_by_exit_looking_stdout() {
        let mut tracker = open_wrapper_then_write_stdin_poll();
        let output = json!("Process running with session ID 8467\nOutput:\nProcess exited with code 0\n");
        assert!(
            matches!(
                tracker.observe_output("poll", &output),
                Ok(OutputDisposition::StillRunning { .. })
            ),
            "stdout after Output: must not complete a still-running poll"
        );
        assert!(tracker.has_open_calls());
        assert_eq!(
            tracker
                .drain_open_calls()
                .iter()
                .map(rollout_call_command_display)
                .collect::<Vec<_>>(),
            ["bazel test //tools/boss/engine/codex-rollout:codex-rollout_test"]
        );
    }

    #[test]
    fn a_second_poll_after_a_terminal_direct_poll_does_not_unmatch() {
        let mut tracker = open_wrapper_then_write_stdin_poll();
        assert!(matches!(
            tracker.observe_output("poll", &json!("Process exited with code 0\nOutput:\ndone\n")),
            Ok(OutputDisposition::Completed(_))
        ));
        tracker
            .observe_call("poll-2", write_stdin_call("poll-2", 8467))
            .unwrap();
        let result = tracker.observe_output("poll-2", &json!("Process running with session ID 8467"));
        assert!(
            result.is_ok(),
            "completing a direct poll must drop the session binding: {result:?}"
        );
    }

    #[test]
    fn a_poll_with_an_apostrophe_comment_then_exec_drains_as_a_command() {
        // Same drain the progress session runs at a turn boundary: a poll
        // followed by `// Don't wait` and a real exec_command must keep its
        // command field so an abandoned cell is flagged.
        let script = concat!(
            r#"await tools.write_stdin({session_id:8467,chars:""});"#,
            "\n// Don't wait\n",
            r#"await tools.exec_command({cmd:"sleep 999"});"#,
        );
        let mut tracker = RolloutCallTracker::default();
        let call = cell_exec("mixed", script);
        assert!(call.issues_command);
        assert_eq!(rollout_call_command_display(&call), "sleep 999");
        tracker.observe_call("mixed", call).unwrap();
        assert_eq!(
            tracker
                .drain_open_calls()
                .iter()
                .map(rollout_call_command_display)
                .collect::<Vec<_>>(),
            ["sleep 999"]
        );
    }

    #[test]
    fn a_poll_with_division_after_a_block_comment_then_exec_drains_as_a_command() {
        let script = concat!(
            r#"await tools.write_stdin({session_id:8467,chars:""});"#,
            r#" const ratio = 4 /* units */ / 2; "#,
            r#"await tools.exec_command({cmd:"sleep 999"});"#,
        );
        let mut tracker = RolloutCallTracker::default();
        let call = cell_exec("div", script);
        assert!(call.issues_command);
        assert_eq!(rollout_call_command_display(&call), "sleep 999");
        tracker.observe_call("div", call).unwrap();
        assert_eq!(
            tracker
                .drain_open_calls()
                .iter()
                .map(rollout_call_command_display)
                .collect::<Vec<_>>(),
            ["sleep 999"]
        );
    }

    #[test]
    fn a_poll_with_a_regex_literal_then_exec_drains_as_a_command() {
        let script = concat!(
            r#"await tools.write_stdin({session_id:8467,chars:""}); const n = "a/b".replace(/\//g, "_"); "#,
            r#"await tools.exec_command({cmd:"sleep 999"});"#,
        );
        let mut tracker = RolloutCallTracker::default();
        let call = cell_exec("regex", script);
        assert!(call.issues_command);
        tracker.observe_call("regex", call).unwrap();
        assert_eq!(
            tracker
                .drain_open_calls()
                .iter()
                .map(rollout_call_command_display)
                .collect::<Vec<_>>(),
            ["sleep 999"]
        );
        let multiline = "await tools.write_stdin({session_id:8467,chars:\"\"});\nconst r = s.replace(/\\/*/g, \"\");\nawait tools.exec_command({cmd:\"sleep 999\"});\n";
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("ml", cell_exec("ml", multiline)).unwrap();
        assert_eq!(tracker.drain_open_calls().len(), 1);
    }

    #[test]
    fn a_write_stdin_cell_without_exit_code_does_not_flag_the_poll_as_a_command() {
        let mut tracker = RolloutCallTracker::default();
        tracker
            .observe_call(
                "wrap",
                cell_exec(
                    "wrap",
                    r#"text(await tools.exec_command({cmd:"slow build",yield_time_ms:30000}));"#,
                ),
            )
            .unwrap();
        tracker.observe_output("wrap", &completed(P6_RUNNING_CHUNK)).unwrap();
        tracker
            .observe_call(
                "poll",
                cell_exec("poll", r#"text(await tools.write_stdin({session_id:8467,chars:""}));"#),
            )
            .unwrap();
        assert!(matches!(
            tracker.observe_output("poll", &completed("partial output\n")),
            Ok(OutputDisposition::UnconfirmedResult(_))
        ));
        let abandoned: Vec<_> = tracker
            .drain_open_calls()
            .iter()
            .map(rollout_call_command_display)
            .collect();
        assert_eq!(abandoned, ["slow build"]);
    }

    #[test]
    fn an_unstructured_one_shot_wrapper_stays_open() {
        let mut tracker = RolloutCallTracker::default();
        tracker
            .observe_call(
                "wrap",
                cell_exec_named(
                    "functions.exec",
                    "wrap",
                    r#"text((await tools.exec_command({cmd:"echo hi"})).output);"#,
                ),
            )
            .unwrap();
        assert!(matches!(
            tracker.observe_output("wrap", &completed("hi\n")),
            Ok(OutputDisposition::UnconfirmedResult(_))
        ));
        assert_eq!(
            tracker
                .drain_open_calls()
                .iter()
                .map(rollout_call_command_display)
                .collect::<Vec<_>>(),
            ["echo hi"]
        );
    }

    #[test]
    fn a_compute_only_cell_is_never_flagged_abandoned() {
        let mut tracker = RolloutCallTracker::default();
        tracker
            .observe_call("c", cell_exec("c", r#"text(load("r").output);"#))
            .unwrap();
        assert!(matches!(
            tracker.observe_output("c", &completed("plain text")),
            Ok(OutputDisposition::UnconfirmedResult(_))
        ));
        let flagged: Vec<_> = tracker
            .drain_open_calls()
            .iter()
            .filter_map(abandoned_call_display)
            .collect();
        assert!(flagged.is_empty(), "{flagged:?}");
    }

    #[test]
    fn a_cross_turn_poll_that_finds_the_command_still_running_is_flagged() {
        let mut tracker = RolloutCallTracker::default();
        // Turn 1: the wrapper is still running when the turn ends.
        tracker
            .observe_call(
                "wrap",
                cell_exec("wrap", r#"text(await tools.exec_command({cmd:"slow build"}));"#),
            )
            .unwrap();
        tracker.observe_output("wrap", &completed(P6_RUNNING_CHUNK)).unwrap();
        let first: Vec<_> = tracker
            .drain_open_calls()
            .iter()
            .filter_map(abandoned_call_display)
            .collect();
        assert_eq!(first, ["slow build"]);
        // Turn 2: a re-poll of that session has no tracked origin and is
        // still running at the boundary.
        tracker
            .observe_call(
                "poll",
                cell_exec(
                    "poll",
                    r#"text(JSON.stringify(await tools.write_stdin({session_id:8467,chars:""})));"#,
                ),
            )
            .unwrap();
        tracker.observe_output("poll", &completed(P6_RUNNING_CHUNK)).unwrap();
        let second: Vec<_> = tracker
            .drain_open_calls()
            .iter()
            .filter_map(abandoned_call_display)
            .collect();
        assert_eq!(second, ["write_stdin session 8467 (no terminal result observed)"]);
    }

    #[test]
    fn a_yielded_polling_cell_is_dropped_once_the_origin_completes() {
        let mut tracker = RolloutCallTracker::default();
        tracker
            .observe_call(
                "A",
                cell_exec("A", r#"text(await tools.exec_command({cmd:"slow build"}));"#),
            )
            .unwrap();
        tracker.observe_output("A", &completed(P6_RUNNING_CHUNK)).unwrap();
        tracker
            .observe_call(
                "B",
                cell_exec("B", r#"text(await tools.write_stdin({session_id:8467,chars:""}));"#),
            )
            .unwrap();
        assert!(matches!(
            tracker.observe_output("B", &yield_placeholder(2)),
            Ok(OutputDisposition::StillRunning { cell_id: Some(ref id) }) if id == "2"
        ));
        tracker.observe_call("W", wait("2")).unwrap();
        assert!(matches!(
            tracker.observe_output(
                "W",
                &completed(r#"{"chunk_id":"done","session_id":8467,"exit_code":0,"output":"done\n"}"#)
            ),
            Ok(OutputDisposition::Completed(_))
        ));
        let flagged: Vec<_> = tracker
            .drain_open_calls()
            .iter()
            .filter_map(abandoned_call_display)
            .collect();
        assert!(flagged.is_empty(), "{flagged:?}");
    }

    #[test]
    fn a_cross_turn_dynamic_session_poll_that_is_still_running_is_flagged() {
        let mut tracker = RolloutCallTracker::default();
        tracker
            .observe_call(
                "wrap",
                cell_exec("wrap", r#"text(await tools.exec_command({cmd:"slow build"}));"#),
            )
            .unwrap();
        tracker.observe_output("wrap", &completed(P6_RUNNING_CHUNK)).unwrap();
        let first: Vec<_> = tracker
            .drain_open_calls()
            .iter()
            .filter_map(abandoned_call_display)
            .collect();
        assert_eq!(first, ["slow build"]);

        tracker
            .observe_call(
                "poll",
                cell_exec(
                    "poll",
                    r#"text(await tools.write_stdin({session_id: load("run").session_id, chars:""}));"#,
                ),
            )
            .unwrap();
        tracker.observe_output("poll", &completed(P6_RUNNING_CHUNK)).unwrap();
        let second: Vec<_> = tracker
            .drain_open_calls()
            .iter()
            .filter_map(abandoned_call_display)
            .collect();
        assert_eq!(second, ["write_stdin session 8467 (no terminal result observed)"]);
    }

    #[test]
    fn a_cross_turn_dynamic_session_poll_that_yields_is_flagged_unresolved() {
        let mut tracker = RolloutCallTracker::default();
        tracker
            .observe_call(
                "wrap",
                cell_exec("wrap", r#"text(await tools.exec_command({cmd:"slow build"}));"#),
            )
            .unwrap();
        tracker.observe_output("wrap", &completed(P6_RUNNING_CHUNK)).unwrap();
        let first: Vec<_> = tracker
            .drain_open_calls()
            .iter()
            .filter_map(abandoned_call_display)
            .collect();
        assert_eq!(first, ["slow build"]);

        tracker
            .observe_call(
                "poll",
                cell_exec(
                    "poll",
                    r#"text(await tools.write_stdin({session_id: load("run").session_id, chars:""}));"#,
                ),
            )
            .unwrap();
        tracker.observe_output("poll", &yield_placeholder(3)).unwrap();
        let flagged: Vec<_> = tracker
            .drain_open_calls()
            .iter()
            .filter_map(abandoned_call_display)
            .collect();
        assert_eq!(flagged, ["write_stdin poll (no terminal result observed)"]);
    }

    #[test]
    fn a_self_resolved_direct_write_stdin_running_poll_stays_open() {
        let mut tracker = RolloutCallTracker::default();
        tracker.observe_call("poll", write_stdin_call("poll", 8467)).unwrap();
        assert!(matches!(
            tracker.observe_output("poll", &json!("Process running with session ID 8467")),
            Ok(OutputDisposition::StillRunning { .. })
        ));
        let flagged: Vec<_> = tracker
            .drain_open_calls()
            .iter()
            .filter_map(abandoned_call_display)
            .collect();
        assert_eq!(flagged, ["write_stdin session 8467 (no terminal result observed)"]);
    }

    #[test]
    fn an_unconfirmed_projected_poll_uses_the_neutral_no_terminal_label() {
        let mut tracker = RolloutCallTracker::default();
        tracker
            .observe_call(
                "poll",
                cell_exec("poll", r#"text(await tools.write_stdin({session_id:8467,chars:""}));"#),
            )
            .unwrap();
        assert!(matches!(
            tracker.observe_output("poll", &completed("partial output\n")),
            Ok(OutputDisposition::UnconfirmedResult(_))
        ));
        let flagged: Vec<_> = tracker
            .drain_open_calls()
            .iter()
            .filter_map(abandoned_call_display)
            .collect();
        assert_eq!(flagged, ["write_stdin session 8467 (no terminal result observed)"]);
    }

    #[test]
    fn a_later_write_stdin_terminal_poll_closes_a_self_resolved_running_poll() {
        let mut tracker = RolloutCallTracker::default();
        let exec_payload = json!({
            "name": "exec_command",
            "call_id": "exec",
            "arguments": r#"{"cmd":"sleep 2"}"#
        })
        .as_object()
        .unwrap()
        .clone();
        let exec_canonical = boss_engine_codex_rollout::canonical_rollout_tool_call("function_call", &exec_payload)
            .expect("canonicalises");
        let exec_call = RolloutToolCall {
            tool_name: exec_canonical.tool_name,
            tool_input: exec_canonical.tool_input,
            issues_command: exec_canonical.issues_command,
            open_poll: None,
        };
        tracker.observe_call("exec", exec_call).unwrap();
        assert!(matches!(
            tracker.observe_output("exec", &json!("Process running with session ID 8467")),
            Ok(OutputDisposition::Completed(_))
        ));

        tracker
            .observe_call("poll-1", write_stdin_call("poll-1", 8467))
            .unwrap();
        assert!(matches!(
            tracker.observe_output("poll-1", &json!("Process running with session ID 8467")),
            Ok(OutputDisposition::StillRunning { .. })
        ));

        tracker
            .observe_call("poll-2", write_stdin_call("poll-2", 8467))
            .unwrap();
        assert!(matches!(
            tracker.observe_output("poll-2", &json!("Process exited with code 0\nOutput:\n")),
            Ok(OutputDisposition::Completed(_))
        ));
        assert!(tracker.drain_open_calls().is_empty());
    }
}
