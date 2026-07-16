//! Engine counter / gauge metrics framework (phase 1).
//!
//! Declaring a new metric is a one- or two-line change at the call
//! site via [`register_counter!`] / [`register_gauge!`]. Values are
//! held in in-memory atomics for the hot path and flushed to
//! `state.db` every 30 seconds (and on graceful shutdown). On engine
//! startup the framework reads the persisted rows back so monotonic
//! counter totals are continuous across restarts.
//!
//! Per the framework design (see
//! `tools/boss/docs/designs/engine-counter-metrics-framework.md`,
//! §"Risks / open questions" item 7) the [`Registry`] is plumbed
//! explicitly as `Arc<Registry>` rather than stashed in a global —
//! every call site takes a `&Registry` so unit tests can construct a
//! local registry without leaking state across tests.
//!
//! Phase 1 ships the registry, the primitives, the `state.db`
//! tables, the flush task and the startup-rehydrate path. Phase 4
//! migrates `DispatcherStats` onto the framework. The `bossctl
//! metrics` surfacing verbs land in a subsequent phase.
//!
//! The registry itself lives in the `boss-engine-metrics-registry`
//! crate and is re-exported here, so declaring or touching a metric
//! doesn't rebuild this crate. [`persistence`] and [`init_all`] stay
//! on this side of the edge: the former needs `crate::work::WorkDb`,
//! the latter reaches into every metric-declaring engine module.

pub mod persistence;

pub use boss_engine_metrics_registry::{CounterHandle, GaugeHandle, Registry, now_ms};
pub use persistence::{flush_all, seed_from_db, spawn_flush_task};

/// Force registration of every counter / gauge handle the engine
/// declares.
///
/// `LazyLock`-style registration would let a counter living in a
/// rarely-loaded module miss its first flush window (and would push
/// the duplicate-name panic from boot into the middle of a busy
/// sweep — see design §"Risks / open questions" item 6, which is
/// load-bearing for item 2). The cure is this single function that
/// touches every handle so registration happens once, deterministically,
/// at engine startup.
///
/// As each new counter module lands, add one line here to register
/// its handles so duplicate-name panics surface at boot rather than
/// at the first increment (design §"Risks / open questions" item 6).
pub fn init_all(registry: &Registry) {
    // Phase 3: PR URL capture path counters.
    crate::completion::register_metrics(registry);
    // Phase 3: Dependency-unblock sweep gauge.
    crate::dep_unblock_sweep::register_metrics(registry);
    // Phase 3: Cube workspace lease counters.
    crate::coordinator::register_metrics(registry);
    // Phase 4: DispatcherStats counters migrated to the framework.
    crate::live_status_loop::register_metrics(registry);
    // Phase 5: SweepOutcome / merge_poller counters.
    crate::merge_poller::init(registry);
    // External tracker reconciler pass counters.
    crate::external_tracker::reconcile::register_metrics(registry);
    // Layer 4 / T10: speculative conflict-prediction sweep counters.
    crate::speculative_conflict::init(registry);
    // Layer 4 / T11: stacked-PR auto-structuring offer counters.
    crate::stacked_pr_structuring::init(registry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_all_registers_all_declared_counters() {
        let registry = Registry::new();
        init_all(&registry);
        let names: Vec<_> = registry.counter_snapshots().into_iter().map(|s| s.name).collect();
        // Phase 3: PR URL capture counters.
        for expected in [
            "pr_url_capture.primary_path.hit",
            "pr_url_capture.reconstruction_path.hit",
            "pr_url_capture.reconstruction_path.failed",
            "pr_url_capture.recheck_staged.branch_mismatch",
        ] {
            assert!(
                names.contains(&expected.to_owned()),
                "init_all must register {expected}"
            );
        }
        // Phase 3: cube workspace lease counters.
        for expected in [
            "cube_workspace_lease.attempts",
            "cube_workspace_lease.success",
            "cube_workspace_lease.failure",
        ] {
            assert!(
                names.contains(&expected.to_owned()),
                "init_all must register {expected}"
            );
        }
        // Phase 4: dispatcher counters.
        assert!(
            names.iter().any(|n| n == "dispatcher.hook_events.total"),
            "expected dispatcher.hook_events.total to be registered; got {names:?}",
        );
        assert!(
            names
                .iter()
                .any(|n| n == "dispatcher.hook_events.for_terminal_execution"),
            "expected dispatcher.hook_events.for_terminal_execution to be registered; got {names:?}",
        );
        // Phase 5: merge_poller counters.
        for expected in [
            "merge_poller.merged",
            "merge_poller.conflict_flagged",
            "merge_poller.conflict_cleared",
            "merge_poller.pr_recheck_recovered",
            "merge_poller.pr_recheck_unresolved",
            "merge_poller.merge_queue_rebounced",
            "merge_poller.late_pr_recovered",
            "merge_poller.revision_invalidated",
            "merge_poller.worker_stopped_on_review",
            "merge_poller.comments_reopened",
        ] {
            assert!(
                names.contains(&expected.to_owned()),
                "init_all must register {expected}"
            );
        }
        // External tracker reconciler counters.
        for expected in [
            "external_tracker.fetch_succeeded",
            "external_tracker.fetch_failed",
            "external_tracker.imported",
            "external_tracker.closed",
            "external_tracker.pr_attached",
            "external_tracker.pr_merge_close_succeeded",
            "external_tracker.pr_merge_close_failed",
            "external_tracker.unbound",
            "external_tracker.skipped_closed_at_first_sight",
            "external_tracker.skip_no_credential",
            "external_tracker.in_progress_set_succeeded",
            "external_tracker.in_progress_set_failed",
            "external_tracker.tracked_label_attach_succeeded",
            "external_tracker.tracked_label_attach_failed",
        ] {
            assert!(
                names.contains(&expected.to_owned()),
                "init_all must register {expected}"
            );
        }
        // Layer 4 / T10: speculative conflict-prediction sweep counters.
        for expected in ["speculative_conflict.predicted", "speculative_conflict.clean"] {
            assert!(
                names.contains(&expected.to_owned()),
                "init_all must register {expected}"
            );
        }
        // Layer 4 / T11: stacked-PR auto-structuring offer counter.
        assert!(
            names.contains(&"stacked_pr_structuring.offered".to_owned()),
            "init_all must register stacked_pr_structuring.offered"
        );
        assert_eq!(
            names.len(),
            48,
            "expected 4 pr_url_capture + 3 cube_workspace_lease + 10 dispatcher + 10 merge_poller + \
             18 external_tracker + 2 speculative_conflict + 1 stacked_pr_structuring counters"
        );
        // Phase 3: dep_unblock gauge.
        let gauge_names: Vec<_> = registry.gauge_snapshots().into_iter().map(|s| s.name).collect();
        assert_eq!(
            gauge_names,
            vec!["dependency_unblock.longest_stale_seconds"],
            "init_all must register the dep_unblock gauge",
        );
    }
}
