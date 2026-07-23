//! Startup registration of every counter / gauge the engine declares.
//!
//! The metrics framework itself lives in the [`boss_metrics`] crate
//! and knows nothing about the engine's modules. Naming them is this
//! module's job — it is the one place that has to reach across the
//! whole engine, which is exactly why it stays here rather than
//! moving down into `boss_metrics`.

use boss_metrics::Registry;

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
    // Queue-level dispatch telemetry: per-pool depth/oldest-wait gauges,
    // dispatch-completed counter, drain-pass-duration gauge.
    crate::dispatch_metrics::register_metrics(registry);
    // Trunk merge-queue poller: probe/lookup volume, state writes, intent
    // retirements, and attention items.
    crate::trunk_queue_poller::init(registry);
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
        // Queue-level dispatch telemetry: dispatch-completed counter.
        assert!(
            names.contains(&"dispatch.completed".to_owned()),
            "init_all must register dispatch.completed"
        );
        // Trunk merge-queue poller counters.
        for expected in [
            "trunk_queue_poller.queue_probes",
            "trunk_queue_poller.queue_probe_failures",
            "trunk_queue_poller.entry_lookups",
            "trunk_queue_poller.state_writes",
            "trunk_queue_poller.intents_retired",
            "trunk_queue_poller.attentions_filed",
            "trunk_queue_poller.evictions_detected",
        ] {
            assert!(
                names.contains(&expected.to_owned()),
                "init_all must register {expected}"
            );
        }
        assert_eq!(
            names.len(),
            56,
            "expected 4 pr_url_capture + 3 cube_workspace_lease + 10 dispatcher + 10 merge_poller + \
             18 external_tracker + 2 speculative_conflict + 1 stacked_pr_structuring + 1 dispatch_metrics + \
             7 trunk_queue_poller counters"
        );
        // Phase 3: dep_unblock gauge, plus the queue-level dispatch gauges.
        let gauge_names: Vec<_> = registry.gauge_snapshots().into_iter().map(|s| s.name).collect();
        assert_eq!(
            gauge_names,
            vec![
                "dependency_unblock.longest_stale_seconds",
                "dispatch.drain_pass_duration_ms",
                "dispatch.queue_depth.automation",
                "dispatch.queue_depth.main",
                "dispatch.queue_depth.review",
                "dispatch.queue_oldest_wait_seconds.automation",
                "dispatch.queue_oldest_wait_seconds.main",
                "dispatch.queue_oldest_wait_seconds.review",
            ],
            "init_all must register the dep_unblock gauge and the queue-level dispatch gauges",
        );
    }
}
