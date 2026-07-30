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
//! [`RolloutCallTracker`] closes both by making the **cell**, not the
//! record, the unit of correlation: a yielded call stays open, keyed by its
//! cell id, until a continuation delivers a terminal result. The call it
//! then completes is the *originating* call, so downstream consumers see
//! the real command paired with the real output and never learn that a
//! `wait` was involved.
//!
//! Correlation failures are [`CorrelationError`]s, never a silent drop: a
//! `wait` naming a cell this session never yielded means the tracker has
//! lost the thread, and a consumer that returned "no result" there would
//! hide exactly the gap this module exists to expose.

use std::collections::{HashMap, VecDeque};

use boss_engine_codex_rollout::cell::{CellOutcome, is_cell_wait_tool, parse_cell_output, wait_target_cell_id};
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
    /// The cell yielded and its command is still running: nothing is
    /// observed yet, and the call stays open.
    StillRunning,
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
/// All three maps are carried, not just the open calls: a cell binding dropped
/// across a restart would make the next `wait` name a cell the tracker never
/// saw yield, which [`RolloutCallTracker::observe_call`] reports as a
/// correlation failure — a real signal that would then be fired by the restart
/// rather than by a genuine loss of the thread.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct RolloutCallSnapshot {
    open_calls: Vec<(String, RolloutToolCall)>,
    cells: Vec<(String, String)>,
    waits: Vec<(String, String)>,
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
    /// `wait` call id → originating call id.
    waits: BoundedMap<String>,
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
            self.calls.insert(call_id.to_owned(), call);
            return Ok(CallDisposition::Announce);
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
        self.waits.insert(call_id.to_owned(), origin);
        Ok(CallDisposition::Continuation)
    }

    /// Resolve a tool-output record against the call that issued the command.
    ///
    /// `output` is the record's verbatim `payload.output` value.
    pub(super) fn observe_output(
        &mut self,
        call_id: &str,
        output: &Value,
    ) -> Result<OutputDisposition, CorrelationError> {
        let origin = self.waits.remove(call_id).unwrap_or_else(|| call_id.to_owned());
        if !self.calls.contains_key(&origin) {
            return Err(CorrelationError::UnmatchedOutput(call_id.to_owned()));
        }
        if let Some(CellOutcome::Yielded { cell_id }) = cell_outcome(output) {
            self.cells.insert(cell_id, origin);
            return Ok(OutputDisposition::StillRunning);
        }
        let call = self
            .calls
            .remove(&origin)
            .expect("origin call presence was just checked");
        Ok(OutputDisposition::Completed(call))
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
        self.waits.clear();
        self.calls.drain_in_order()
    }

    /// Capture this tracker's correlation state for a later readoption.
    pub(super) fn snapshot(&self) -> RolloutCallSnapshot {
        RolloutCallSnapshot {
            open_calls: self.calls.entries_in_order(),
            cells: self.cells.entries_in_order(),
            waits: self.waits.entries_in_order(),
        }
    }

    /// Re-seed this tracker from a snapshot, replacing whatever it held.
    pub(super) fn restore(&mut self, snapshot: RolloutCallSnapshot) {
        self.calls.restore_in_order(snapshot.open_calls);
        self.cells.restore_in_order(snapshot.cells);
        self.waits.restore_in_order(snapshot.waits);
    }

    /// Forget everything — a new session owns none of the previous one's
    /// correlation state.
    pub(super) fn reset(&mut self) {
        self.calls.clear();
        self.cells.clear();
        self.waits.clear();
    }
}

/// Classify a tool-output record as a cell-harness envelope, if it is one.
fn cell_outcome(output: &Value) -> Option<CellOutcome> {
    parse_cell_output(&flatten_tool_output_text(output)).map(|parsed| parsed.outcome)
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
        }
    }

    fn wait(cell_id: &str) -> RolloutToolCall {
        RolloutToolCall {
            tool_name: "wait".to_owned(),
            tool_input: json!({"cell_id": cell_id, "yield_time_ms": 30000}),
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

    #[test]
    fn wait_continuation_completes_the_originating_call() {
        let mut tracker = RolloutCallTracker::default();
        assert!(matches!(
            tracker.observe_call("A", bash("cube pr create --branch b")),
            Ok(CallDisposition::Announce)
        ));
        assert!(matches!(
            tracker.observe_output("A", &yield_placeholder(1)),
            Ok(OutputDisposition::StillRunning)
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
            Ok(OutputDisposition::StillRunning)
        ));
        assert!(tracker.has_open_calls());
        tracker.observe_call("W2", wait("2")).unwrap();
        let Ok(OutputDisposition::Completed(call)) = tracker.observe_output("W2", &completed("{}")) else {
            panic!("the second continuation must complete the originating call");
        };
        assert_eq!(
            call.tool_input.get("command").and_then(Value::as_str),
            Some("slow build")
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
}
