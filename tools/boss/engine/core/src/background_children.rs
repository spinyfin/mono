//! Live-descendant-process probe for a worker's Stop boundary.
//!
//! Background: observed live 2026-07-17 — a worker's Claude session had
//! spawned one or more BACKGROUND SUBAGENTS via the harness Agent tool
//! (`subagent_type: "fork"` and friends) — separate `claude` processes
//! that re-invoke the worker with a task-notification once they finish.
//! Between the worker's own Stop (its *turn* genuinely ended — it is
//! waiting on the subagent) and that notification, the engine sees pure
//! hook silence: exactly the signature [`crate::nudge_breaker`] and
//! [`crate::completion::WorkerCompletionHandler::nudge_or_park`] treat as
//! a stall, so a worker legitimately waiting on delegated work could be
//! nudged into unproductive replies and eventually parked/abandoned.
//!
//! Same misclassification family as the build-wait false positives
//! ([`crate::build_wait`]) — a worker that ended its turn
//! for a legitimate reason looks identical, over hooks alone, to one that
//! is truly stuck — but a distinct case: there the worker's own
//! foreground process is busy (mid-tool-call); here the worker's turn has
//! genuinely ended and its process tree simply still contains live
//! descendant processes doing delegated work.
//!
//! Process groups were initially used as a name-free discriminator, backed
//! by a `libproc`-based process-tree walk. Live Codex and Claude trees
//! disproved that premise: driver helpers and ordinary tool subprocesses may
//! create their own process groups just like delegated work, so a process
//! table cannot honestly distinguish those two kinds of child. The pgid walk
//! was removed as a result — [`RegistryBackgroundActivityProbe`] now fails
//! open unconditionally, reporting every Stop as indeterminate, until a
//! driver/harness-owned delegated-work signal is available to replace it
//! (see `[deferred-scope]` in the PR that landed this). The trait and
//! horizon/recheck machinery below stay in place so that future signal has
//! somewhere to plug in; the pgid-specific walk itself does not survive this
//! revision — see git history (`count_with_process_table`) if it is ever
//! needed as a reference.
//!
//! Once a real signal exists, a worker with delegated descendants is
//! WAITING, not stalled — the same time-bounded suppression pattern
//! build-wait uses ([`crate::build_wait_tracker::BuildWaitTracker`]) bounds
//! how long that trust lasts, so a descendant that never exits (a genuinely
//! wedged subagent) still eventually surfaces to the normal nudge/park flow
//! rather than being trusted forever.

/// Default horizon a continuously-reported live-descendant sighting is
/// trusted for, measured from the first reported sighting, before
/// [`crate::completion::WorkerCompletionHandler::nudge_or_park`] stops
/// suppressing and falls back to the normal nudge/park flow. Reuses
/// [`crate::build_wait_tracker::DEFAULT_BUILD_WAIT_HORIZON_SECS`]'s value
/// (45 minutes) — the same reasoning applies: comfortably longer than
/// [`crate::stale_worker_sweep::DEFAULT_STALE_THRESHOLD_SECS`] while still
/// bounding an indefinite suppression should a subagent process genuinely
/// wedge instead of exiting.
pub const DEFAULT_BACKGROUND_CHILDREN_HORIZON_SECS: i64 = crate::build_wait_tracker::DEFAULT_BUILD_WAIT_HORIZON_SECS;

/// Reports whether an execution's worker process tree still has live
/// descendant processes at Stop boundary — the signal
/// [`crate::completion::WorkerCompletionHandler::nudge_or_park`] uses to
/// distinguish "waiting on delegated work" from "genuinely idle". See the
/// module doc comment for the incident this exists to fix.
pub trait BackgroundActivityProbe: Send + Sync {
    /// Number of live delegated descendants for the worker backing
    /// `execution_id`. An unresolved worker or indeterminate process-tree
    /// classification is an error so the caller can fail loudly and nudge.
    fn live_delegated_descendant_count(&self, execution_id: &str) -> Result<usize, String>;

    /// Opaque hook-activity watermark for avoiding a recurring probe in the
    /// middle of a worker's resumed turn. Implementations without hook state
    /// return `None`.
    fn activity_watermark(&self, _execution_id: &str) -> Option<String> {
        None
    }
}

/// Default probe that always reports zero descendants. Used as the
/// [`crate::completion::WorkerCompletionHandler`] default so test sites
/// that don't wire in a real probe get the historical behaviour
/// (background-children suppression never fires).
pub struct NoopBackgroundActivityProbe;

impl BackgroundActivityProbe for NoopBackgroundActivityProbe {
    fn live_delegated_descendant_count(&self, _execution_id: &str) -> Result<usize, String> {
        Ok(0)
    }
}

/// Production probe: resolves `execution_id` to its live worker's shell pid
/// via [`crate::live_worker_state::LiveWorkerStateRegistry`], then reports
/// indeterminate unconditionally — see the module doc for why the pgid
/// discriminator this used to classify descendants was removed. Reporting
/// indeterminate (rather than a count) is what keeps every Stop on the
/// normal nudge/park path instead of wrongly suppressing a real nudge.
pub struct RegistryBackgroundActivityProbe {
    live_worker_states: std::sync::Arc<crate::live_worker_state::LiveWorkerStateRegistry>,
}

impl RegistryBackgroundActivityProbe {
    pub fn new(live_worker_states: std::sync::Arc<crate::live_worker_state::LiveWorkerStateRegistry>) -> Self {
        Self { live_worker_states }
    }
}

impl BackgroundActivityProbe for RegistryBackgroundActivityProbe {
    fn live_delegated_descendant_count(&self, execution_id: &str) -> Result<usize, String> {
        let Some(shell_pid) = self.live_worker_states.shell_pid_for_run(execution_id) else {
            return Err(format!("no live shell pid registered for execution {execution_id}"));
        };
        if shell_pid <= 0 {
            return Err(format!("invalid shell pid {shell_pid} for execution {execution_id}"));
        }
        // Do not classify descendants by pgid: Codex's code-mode host and
        // Claude Bash tools both legitimately run in their own group, which
        // is indistinguishable from a delegated child.
        Err(format!(
            "process-table descendants cannot distinguish driver helpers from delegated work (shell pid {shell_pid})"
        ))
    }

    fn activity_watermark(&self, execution_id: &str) -> Option<String> {
        self.live_worker_states.activity_watermark_for_run(execution_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_probe_always_fails_open() {
        // The pgid discriminator was removed (see module doc): a live shell
        // pid is not sufficient to tell driver helpers from delegated work,
        // so the production probe must always report indeterminate, never a
        // count — that is what keeps `BackgroundChildrenPending` from firing
        // on a driver helper and wrongly suppressing a real nudge.
        let registry = std::sync::Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
        registry.register_spawn(1, "exec_a", "opus", 4242, None);
        let probe = RegistryBackgroundActivityProbe::new(registry);
        assert!(probe.live_delegated_descendant_count("exec_a").is_err());
    }

    struct FixedProbe(usize);
    impl BackgroundActivityProbe for FixedProbe {
        fn live_delegated_descendant_count(&self, _execution_id: &str) -> Result<usize, String> {
            Ok(self.0)
        }
    }

    #[test]
    fn noop_probe_always_reports_zero() {
        let probe = NoopBackgroundActivityProbe;
        assert_eq!(probe.live_delegated_descendant_count("exec_a"), Ok(0));
    }

    #[test]
    fn fixed_probe_reports_configured_count() {
        let probe = FixedProbe(3);
        assert_eq!(probe.live_delegated_descendant_count("exec_a"), Ok(3));
    }

    #[test]
    fn registry_probe_fails_for_unknown_execution() {
        let registry = std::sync::Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new());
        let probe = RegistryBackgroundActivityProbe::new(registry);
        assert!(probe.live_delegated_descendant_count("exec_unknown").is_err());
    }
}
