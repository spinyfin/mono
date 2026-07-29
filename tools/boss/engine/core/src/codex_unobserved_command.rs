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

/// Bound on distinct commands remembered per execution — caps a
/// pathological run that abandons an unbounded number of commands rather
/// than growing the staging map without limit.
const MAX_COMMANDS_PER_EXECUTION: usize = 50;

/// In-memory `execution_id → abandoned commands` staging map. Populated by
/// the worker-event dispatcher in `app/worker_events.rs` when a
/// `WorkerEvent::Notification` carries
/// [`boss_engine_driver::codex::UNOBSERVED_COMMAND_MARKER`]; read (never
/// cleared mid-run) by [`crate::completion::WorkerCompletionHandler`]'s
/// unobserved-command pass and its `NO_CHANGES_NEEDED` refusal gate.
///
/// Unlike [`crate::proposal_channel_error::ProposalChannelErrorTracker`]'s
/// first-writer-wins single slot, this accumulates: every distinct abandoned
/// command staged for an execution stays visible for the lifetime of the
/// run, because the question this tracker answers — "did this execution
/// ever leave a command unobserved?" — only ever gets more true, never less,
/// as the run continues.
#[derive(Debug, Default)]
pub struct UnobservedCommandTracker {
    inner: Mutex<HashMap<String, Vec<String>>>,
}

impl UnobservedCommandTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage one abandoned command for `execution_id`. Returns whether it
    /// was newly recorded — `false` for a command already staged (the same
    /// cumulative signal re-observed, e.g. a duplicate hook delivery) or once
    /// [`MAX_COMMANDS_PER_EXECUTION`] is reached.
    pub fn record(&self, execution_id: &str, command: &str) -> bool {
        let mut guard = self.inner.lock().expect("UnobservedCommandTracker mutex poisoned");
        let commands = guard.entry(execution_id.to_owned()).or_default();
        if commands.iter().any(|existing| existing == command) {
            return false;
        }
        if commands.len() >= MAX_COMMANDS_PER_EXECUTION {
            return false;
        }
        commands.push(command.to_owned());
        true
    }

    /// Every abandoned command staged for `execution_id`, oldest first. Does
    /// not clear — callers dedupe against already-filed attentions instead,
    /// so this can be consulted safely on every Stop of a run.
    pub fn list(&self, execution_id: &str) -> Vec<String> {
        self.inner
            .lock()
            .expect("UnobservedCommandTracker mutex poisoned")
            .get(execution_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether `execution_id` has ever staged an abandoned command — the
    /// check `on_stop_inner`'s `NO_CHANGES_NEEDED` gate uses to refuse the
    /// worker's no-op claim.
    pub fn has_any(&self, execution_id: &str) -> bool {
        self.inner
            .lock()
            .expect("UnobservedCommandTracker mutex poisoned")
            .get(execution_id)
            .is_some_and(|commands| !commands.is_empty())
    }
}

crate::register_counter!(
    CODEX_UNOBSERVED_COMMAND,
    "codex.unobserved_command",
    "A Codex command_execution item.started (or rollout function_call) was observed with no \
     matching completion before the turn boundary; the worker's completion claims for that run \
     are treated as unconfirmed.",
);

/// Register the unobserved-command counter handle with `registry`. Called
/// from [`crate::metrics_init::init_all`] at engine startup.
pub fn register_metrics(registry: &crate::metrics::Registry) {
    registry.register_counter(&CODEX_UNOBSERVED_COMMAND);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_lists_distinct_commands() {
        let tracker = UnobservedCommandTracker::new();
        assert!(tracker.record("exec_1", "sleep 999"));
        assert!(tracker.record("exec_1", "curl slow-endpoint"));
        assert_eq!(tracker.list("exec_1"), vec!["sleep 999", "curl slow-endpoint"]);
    }

    #[test]
    fn duplicate_command_is_not_restaged() {
        let tracker = UnobservedCommandTracker::new();
        assert!(tracker.record("exec_1", "sleep 999"));
        assert!(!tracker.record("exec_1", "sleep 999"));
        assert_eq!(tracker.list("exec_1"), vec!["sleep 999"]);
    }

    #[test]
    fn has_any_is_false_until_something_is_staged() {
        let tracker = UnobservedCommandTracker::new();
        assert!(!tracker.has_any("exec_1"));
        tracker.record("exec_1", "sleep 999");
        assert!(tracker.has_any("exec_1"));
        assert!(!tracker.has_any("exec_2"));
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
    fn per_execution_cap_bounds_pathological_runs() {
        let tracker = UnobservedCommandTracker::new();
        for i in 0..MAX_COMMANDS_PER_EXECUTION {
            assert!(tracker.record("exec_1", &format!("cmd-{i}")));
        }
        assert!(!tracker.record("exec_1", "cmd-overflow"));
        assert_eq!(tracker.list("exec_1").len(), MAX_COMMANDS_PER_EXECUTION);
    }
}
