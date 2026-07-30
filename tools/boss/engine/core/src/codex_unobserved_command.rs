//! Detection of Codex "unobserved commands": a `command_execution` item
//! whose start record (`item.started` on the stdout dialect, a rollout
//! `function_call`/`custom_tool_call`) was observed but no matching
//! completion (`item.completed` / `function_call_output`) ever arrived
//! before the turn boundary. Reproduced in probe 6 of the exit-code
//! investigation: a shell command that outlives the model's chosen
//! `yield_time_ms` with no further polling leaves `turn.completed` firing
//! (and `codex exec` exiting 0) with no completion record for the command
//! anywhere.
//!
//! [`boss_engine_driver::codex`]'s progress-session normalisers detect the
//! gap structurally — they already correlate start/complete pairs to emit
//! `PreToolUse`/`PostToolUse` — and surface it as a `WorkerEvent::Notification`
//! carrying [`boss_engine_driver::codex::UNOBSERVED_COMMAND_MARKER`] ahead of
//! the `Stop` it precedes in the same normalised batch. The dispatcher in
//! `app/worker_events.rs` stages it here (mirroring
//! [`crate::proposal_channel_error`]'s staging pattern); `on_stop_inner`
//! consumes the staged state to file an attention item AND to refuse the
//! worker's `NO_CHANGES_NEEDED` ("validation passed, nothing to do") claim
//! for the rest of the run — an unobserved command means Boss never actually
//! confirmed that command's outcome, so a downstream claim resting on it is
//! unconfirmed, not verified.

use std::collections::HashMap;
use std::sync::Mutex;

/// `work_attention_items.kind` for a filed unobserved-command attention.
/// Mirrors [`crate::deferred_scope::DEFERRED_SCOPE_ATTENTION_KIND`]'s pattern
/// of one constant per marker-class attention kind.
pub const UNOBSERVED_COMMAND_ATTENTION_KIND: &str = "codex_unobserved_command";

/// `work_attention_items.kind` for the one-time "audit trail overflowed"
/// attention — distinct from [`UNOBSERVED_COMMAND_ATTENTION_KIND`] because it
/// reports a different fact (Boss stopped being able to name every abandoned
/// command for this execution) rather than one more abandoned command.
pub const UNOBSERVED_COMMAND_OVERFLOW_ATTENTION_KIND: &str = "codex_unobserved_command_overflow";

/// Bound on distinct commands kept in the per-execution **audit trail**
/// (the list [`UnobservedCommandTracker::list`] returns for attention-item
/// filing) — caps a pathological run that abandons an unbounded number of
/// *distinct* commands rather than growing that list without limit. This is
/// a whole-session-lifetime bound on record-keeping, not a turn bound: 50
/// distinct abandoned commands across an entire multi-turn Codex session is
/// already a deeply pathological run, so the original sizing holds — what
/// was wrong (see [`UnobservedCommandTracker`]) was never this cap, it was
/// letting the list's mere non-emptiness gate `NO_CHANGES_NEEDED` forever.
/// Hitting this cap no longer degrades that gate at all (`unresolved` below
/// is tracked independently of it) — it only stops growing the audit trail,
/// and does so loudly: [`RecordOutcome::CapExceeded`] is logged at `error`
/// and counted on [`CODEX_UNOBSERVED_COMMAND_OVERFLOW`] by the caller in
/// `app/worker_events.rs`, and [`UnobservedCommandTracker::overflowed`]
/// drives a dedicated, one-time attention item filed by
/// [`crate::completion::WorkerCompletionHandler::detect_and_file_unobserved_command_signal`].
pub(crate) const MAX_COMMANDS_PER_EXECUTION: usize = 50;

/// Outcome of [`UnobservedCommandTracker::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Newly added to the audit trail.
    Staged,
    /// The same command text is already in the audit trail for this
    /// execution — the same cumulative signal re-observed (e.g. a duplicate
    /// hook delivery).
    Duplicate,
    /// [`MAX_COMMANDS_PER_EXECUTION`] was already reached; the command is
    /// NOT added to the audit trail. The completion-gate signal
    /// (`unresolved`) is still set — overflowing the audit trail must never
    /// silently weaken the `NO_CHANGES_NEEDED` refusal it feeds.
    CapExceeded,
}

/// Per-execution state. Two independent views over the same abandoned-command
/// stream, because they answer two different questions with two different
/// lifetimes:
///
/// - `commands` is the permanent, capped **audit trail** — every distinct
///   abandoned command this execution has ever left unobserved, for
///   [`crate::completion::WorkerCompletionHandler::detect_and_file_unobserved_command_signal`]
///   to file (once) as an attention item. It never clears and is bounded by
///   [`MAX_COMMANDS_PER_EXECUTION`].
/// - `unresolved` is the **completion-gate signal**: "has an abandoned
///   command been staged since the gate last consumed this flag?" It is
///   uncapped (setting it never fails) and self-clearing — see
///   [`UnobservedCommandTracker::consume_unresolved`].
#[derive(Debug, Default)]
struct ExecutionState {
    commands: Vec<String>,
    unresolved: bool,
    overflowed: bool,
}

/// In-memory `execution_id → abandoned commands` staging map. Populated by
/// the worker-event dispatcher in `app/worker_events.rs` when a
/// `WorkerEvent::Notification` carries
/// [`boss_engine_driver::codex::UNOBSERVED_COMMAND_MARKER`]; read by
/// [`crate::completion::WorkerCompletionHandler`]'s unobserved-command pass
/// (the permanent audit trail, via [`Self::list`]) and its `NO_CHANGES_NEEDED`
/// refusal gate (the self-clearing signal, via [`Self::consume_unresolved`]).
///
/// **Why this is two views, not [`crate::proposal_channel_error::ProposalChannelErrorTracker`]'s
/// single accumulate-forever slot.** The Codex driver now runs a long-lived
/// multi-turn session (`tools/boss/docs/designs/codex-as-a-first-class-agent-driver.md`),
/// and it emits a `WorkerEvent::Stop` at *every* turn boundary
/// (`tools/boss/engine/driver/src/codex/progress.rs`'s `task_complete` /
/// `turn_aborted` arms), not once at process exit. The old reasoning — "did
/// this execution ever leave a command unobserved?" only ever gets more
/// true, never less" — was sound only when a run had exactly one `Stop`: in
/// that world "ever" and "as of the terminal Stop" were the same question.
/// Under a session with many `Stop`s it stopped being the same question: an
/// abandoned command staged on turn 1 made `has_any` (the old, single-view
/// API this replaced) permanently `true`, so it refused *every* later
/// `NO_CHANGES_NEEDED` claim for the rest of the run — turns 2..N included,
/// however cleanly they completed, and with no way for the run to ever
/// recover trust.
///
/// The corrected question the gate must ask is scoped to what it can
/// actually vouch for: "has this run left a command unobserved *since the
/// gate last acted on that fact*?" The first `NO_CHANGES_NEEDED` claim after
/// an abandonment is still correctly refused — Boss cannot confirm that
/// command's outcome, so it falls through to the normal produce-a-PR nudge,
/// exactly as before. What no longer happens is every *subsequent*,
/// unrelated claim inheriting that same refusal for the rest of the session.
/// This does not soften the refusal condition itself (still: an
/// unconfirmed command makes the immediately-following no-op claim
/// untrustworthy) — it only fixes which claims that condition reaches, per
/// the project brief's "fix which conditions reach it" constraint. The
/// permanent audit trail (`commands`, filed as attention items) is
/// unaffected and keeps every abandonment visible to a human reviewer for
/// the life of the run, regardless of how many times the gate has since
/// reset.
#[derive(Debug, Default)]
pub struct UnobservedCommandTracker {
    inner: Mutex<HashMap<String, ExecutionState>>,
}

impl UnobservedCommandTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage one abandoned command for `execution_id`. Always arms
    /// [`Self::consume_unresolved`] for this execution (even on
    /// [`RecordOutcome::Duplicate`] / [`RecordOutcome::CapExceeded`] — the
    /// gate signal is uncapped and must never be weakened by audit-trail
    /// bookkeeping), and appends to the capped audit trail unless the
    /// command text is already present or [`MAX_COMMANDS_PER_EXECUTION`] is
    /// reached.
    pub fn record(&self, execution_id: &str, command: &str) -> RecordOutcome {
        let mut guard = self.inner.lock().expect("UnobservedCommandTracker mutex poisoned");
        let state = guard.entry(execution_id.to_owned()).or_default();
        state.unresolved = true;
        if state.commands.iter().any(|existing| existing == command) {
            return RecordOutcome::Duplicate;
        }
        if state.commands.len() >= MAX_COMMANDS_PER_EXECUTION {
            state.overflowed = true;
            return RecordOutcome::CapExceeded;
        }
        state.commands.push(command.to_owned());
        RecordOutcome::Staged
    }

    /// Every abandoned command staged for `execution_id`, oldest first,
    /// bounded at [`MAX_COMMANDS_PER_EXECUTION`]. Does not clear — callers
    /// dedupe against already-filed attentions instead, so this can be
    /// consulted safely on every Stop of a run.
    pub fn list(&self, execution_id: &str) -> Vec<String> {
        self.inner
            .lock()
            .expect("UnobservedCommandTracker mutex poisoned")
            .get(execution_id)
            .map(|state| state.commands.clone())
            .unwrap_or_default()
    }

    /// Whether the audit trail for `execution_id` has ever overflowed
    /// [`MAX_COMMANDS_PER_EXECUTION`] — drives the one-time overflow
    /// attention item in
    /// [`crate::completion::WorkerCompletionHandler::detect_and_file_unobserved_command_signal`].
    /// Sticky (never clears): the operator-visible record that this
    /// execution abandoned more distinct commands than Boss keeps a written
    /// trail for must survive even after the trail itself stops growing.
    pub fn overflowed(&self, execution_id: &str) -> bool {
        self.inner
            .lock()
            .expect("UnobservedCommandTracker mutex poisoned")
            .get(execution_id)
            .is_some_and(|state| state.overflowed)
    }

    /// Read-and-clear: has an abandoned command been staged for
    /// `execution_id` since the last call to this method (or since the
    /// execution's first staged command, if this is the first call)? This is
    /// the `NO_CHANGES_NEEDED` refusal gate's signal — see the type-level
    /// doc for why it must self-clear rather than latch for the life of the
    /// run.
    pub fn consume_unresolved(&self, execution_id: &str) -> bool {
        let mut guard = self.inner.lock().expect("UnobservedCommandTracker mutex poisoned");
        match guard.get_mut(execution_id) {
            Some(state) => std::mem::take(&mut state.unresolved),
            None => false,
        }
    }
}

crate::register_counter!(
    CODEX_UNOBSERVED_COMMAND,
    "codex.unobserved_command",
    "A Codex command_execution item.started (or rollout function_call) was observed with no \
     matching completion before the turn boundary; the worker's completion claims for that run \
     are treated as unconfirmed.",
);

crate::register_counter!(
    CODEX_UNOBSERVED_COMMAND_OVERFLOW,
    "codex.unobserved_command_overflow",
    "A Codex execution abandoned more than MAX_COMMANDS_PER_EXECUTION distinct commands in one \
     run; the audit trail stopped growing (the NO_CHANGES_NEEDED refusal gate is unaffected — it \
     does not depend on this cap).",
);

/// Register the unobserved-command counter handles with `registry`. Called
/// from [`crate::metrics_init::init_all`] at engine startup.
pub fn register_metrics(registry: &crate::metrics::Registry) {
    registry.register_counter(&CODEX_UNOBSERVED_COMMAND);
    registry.register_counter(&CODEX_UNOBSERVED_COMMAND_OVERFLOW);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_lists_distinct_commands() {
        let tracker = UnobservedCommandTracker::new();
        assert_eq!(tracker.record("exec_1", "sleep 999"), RecordOutcome::Staged);
        assert_eq!(tracker.record("exec_1", "curl slow-endpoint"), RecordOutcome::Staged);
        assert_eq!(tracker.list("exec_1"), vec!["sleep 999", "curl slow-endpoint"]);
    }

    #[test]
    fn duplicate_command_is_not_restaged() {
        let tracker = UnobservedCommandTracker::new();
        assert_eq!(tracker.record("exec_1", "sleep 999"), RecordOutcome::Staged);
        assert_eq!(tracker.record("exec_1", "sleep 999"), RecordOutcome::Duplicate);
        assert_eq!(tracker.list("exec_1"), vec!["sleep 999"]);
    }

    #[test]
    fn list_never_clears() {
        let tracker = UnobservedCommandTracker::new();
        tracker.record("exec_1", "sleep 999");
        assert_eq!(tracker.list("exec_1"), vec!["sleep 999"]);
        assert_eq!(
            tracker.list("exec_1"),
            vec!["sleep 999"],
            "a second read must see the same state"
        );
    }

    #[test]
    fn per_execution_cap_bounds_the_audit_trail_loudly() {
        let tracker = UnobservedCommandTracker::new();
        for i in 0..MAX_COMMANDS_PER_EXECUTION {
            assert_eq!(tracker.record("exec_1", &format!("cmd-{i}")), RecordOutcome::Staged);
        }
        assert!(!tracker.overflowed("exec_1"));
        assert_eq!(tracker.record("exec_1", "cmd-overflow"), RecordOutcome::CapExceeded);
        assert_eq!(tracker.list("exec_1").len(), MAX_COMMANDS_PER_EXECUTION);
        assert!(
            tracker.overflowed("exec_1"),
            "hitting the cap must be an operator-visible, sticky fact"
        );
    }

    #[test]
    fn cap_exceeded_still_arms_the_completion_gate() {
        let tracker = UnobservedCommandTracker::new();
        for i in 0..MAX_COMMANDS_PER_EXECUTION {
            tracker.record("exec_1", &format!("cmd-{i}"));
        }
        assert!(tracker.consume_unresolved("exec_1"));
        assert_eq!(tracker.record("exec_1", "cmd-overflow"), RecordOutcome::CapExceeded);
        assert!(
            tracker.consume_unresolved("exec_1"),
            "an audit-trail overflow must not silently weaken the NO_CHANGES_NEEDED refusal gate"
        );
    }

    #[test]
    fn consume_unresolved_self_clears() {
        let tracker = UnobservedCommandTracker::new();
        assert!(
            !tracker.consume_unresolved("exec_1"),
            "nothing staged yet — the gate must not fire on an unknown execution"
        );
        tracker.record("exec_1", "sleep 999");
        assert!(
            tracker.consume_unresolved("exec_1"),
            "the first read after staging must see the unresolved command"
        );
        assert!(
            !tracker.consume_unresolved("exec_1"),
            "a second read with nothing new staged since must NOT re-refuse — this is exactly \
             the multi-turn-session bug: a command abandoned once must not permanently refuse \
             every later NO_CHANGES_NEEDED claim for the rest of the run"
        );
        assert!(
            !tracker.consume_unresolved("exec_2"),
            "unrelated execution must be unaffected"
        );
    }

    #[test]
    fn consume_unresolved_rearms_on_a_later_turn() {
        let tracker = UnobservedCommandTracker::new();
        tracker.record("exec_1", "sleep 999");
        assert!(tracker.consume_unresolved("exec_1"), "turn 1's abandonment is consumed");
        assert!(
            !tracker.consume_unresolved("exec_1"),
            "turn 2 completed cleanly — no new abandonment since the last consume"
        );
        tracker.record("exec_1", "curl slow-endpoint");
        assert!(
            tracker.consume_unresolved("exec_1"),
            "turn 3 abandoned a new command — the gate must fire again"
        );
    }
}
