//! Live-descendant-process probe for a worker's Stop boundary.
//!
//! Background: observed live 2026-07-17 — a worker's Claude session had
//! spawned one or more BACKGROUND SUBAGENTS via the harness Agent tool
//! (`subagent_type: "fork"` and friends) — separate `claude` processes
//! that re-invoke the worker with a task-notification once they finish.
//! Between the worker's own Stop (its *turn* genuinely ended — it is
//! waiting on the subagent) and that notification, the engine sees pure
//! hook silence: exactly the signature [`crate::nudge_breaker`] and
//! [`crate::WorkerCompletionHandler::nudge_or_park`] treat as
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
/// [`crate::WorkerCompletionHandler::nudge_or_park`] stops
/// suppressing and falls back to the normal nudge/park flow. Reuses
/// [`crate::build_wait_tracker::DEFAULT_BUILD_WAIT_HORIZON_SECS`]'s value
/// (45 minutes) — the same reasoning applies: comfortably longer than
/// [`boss_engine_storage_values::pool_policy::DEFAULT_STALE_THRESHOLD_SECS`] while still
/// bounding an indefinite suppression should a subagent process genuinely
/// wedge instead of exiting.
pub const DEFAULT_BACKGROUND_CHILDREN_HORIZON_SECS: i64 = crate::build_wait_tracker::DEFAULT_BUILD_WAIT_HORIZON_SECS;

/// Reports whether an execution's worker process tree still has live
/// descendant processes at Stop boundary — the signal
/// [`crate::WorkerCompletionHandler::nudge_or_park`] uses to
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
/// [`crate::WorkerCompletionHandler`] default so test sites
/// that don't wire in a real probe get the historical behaviour
/// (background-children suppression never fires).
pub struct NoopBackgroundActivityProbe;

impl BackgroundActivityProbe for NoopBackgroundActivityProbe {
    fn live_delegated_descendant_count(&self, _execution_id: &str) -> Result<usize, String> {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
