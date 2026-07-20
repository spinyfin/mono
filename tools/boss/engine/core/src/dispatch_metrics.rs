//! Queue-level dispatch telemetry: ready-queue depth per pool, oldest-
//! ready wait age per pool, dispatch completion counts, and drain-pass
//! duration.
//!
//! Before this module, the only dispatch-adjacent counters were
//! `cube_workspace_lease.{attempts,success,failure}`
//! ([`crate::coordinator::register_metrics`]) and
//! `dispatcher.hook_events.*` — nothing answered "how backed up is
//! dispatch right now" without scanning `dispatch-events/current.jsonl`
//! by hand (`bossctl dispatch stats`) or reasoning about per-execution
//! JSONL timelines (`bossctl dispatch ghost-active`). These gauges are
//! point-in-time samples fed by [`crate::coordinator::Coordinator`]'s
//! drain loop, persisted through the existing metrics [`Registry`] (see
//! `app/server.rs`'s 30s flush task) so they survive engine restarts and
//! are queryable the same way as every other engine metric.

use crate::metrics::Registry;

crate::register_gauge!(
    DISPATCH_QUEUE_DEPTH_MAIN,
    "dispatch.queue_depth.main",
    "Number of `ready` executions targeting the main pool, sampled at the start of the most recent drain pass.",
);
crate::register_gauge!(
    DISPATCH_QUEUE_DEPTH_AUTOMATION,
    "dispatch.queue_depth.automation",
    "Number of `ready` executions targeting the automation pool, sampled at the start of the most recent drain pass.",
);
crate::register_gauge!(
    DISPATCH_QUEUE_DEPTH_REVIEW,
    "dispatch.queue_depth.review",
    "Number of `ready` executions targeting the review pool, sampled at the start of the most recent drain pass.",
);
crate::register_gauge!(
    DISPATCH_QUEUE_OLDEST_WAIT_SECONDS_MAIN,
    "dispatch.queue_oldest_wait_seconds.main",
    "Age in seconds (by created_at) of the oldest ready main-pool execution, sampled at the start of the most recent drain pass.",
);
crate::register_gauge!(
    DISPATCH_QUEUE_OLDEST_WAIT_SECONDS_AUTOMATION,
    "dispatch.queue_oldest_wait_seconds.automation",
    "Age in seconds (by created_at) of the oldest ready automation-pool execution, sampled at the start of the most recent drain pass.",
);
crate::register_gauge!(
    DISPATCH_QUEUE_OLDEST_WAIT_SECONDS_REVIEW,
    "dispatch.queue_oldest_wait_seconds.review",
    "Age in seconds (by created_at) of the oldest ready review-pool execution, sampled at the start of the most recent drain pass.",
);
crate::register_counter!(
    DISPATCH_COMPLETED,
    "dispatch.completed",
    "Total executions that claimed a worker slot (WorkerClaimed ok) since engine start; rate over a window gives the dispatch rate.",
);
crate::register_gauge!(
    DISPATCH_DRAIN_PASS_DURATION_MS,
    "dispatch.drain_pass_duration_ms",
    "Wall-clock duration in milliseconds of the most recently completed drain_ready_queue pass.",
);

/// Register every handle this module declares. Called from
/// [`crate::metrics_init::init_all`] at engine startup.
pub fn register_metrics(registry: &Registry) {
    registry.register_gauge(&DISPATCH_QUEUE_DEPTH_MAIN);
    registry.register_gauge(&DISPATCH_QUEUE_DEPTH_AUTOMATION);
    registry.register_gauge(&DISPATCH_QUEUE_DEPTH_REVIEW);
    registry.register_gauge(&DISPATCH_QUEUE_OLDEST_WAIT_SECONDS_MAIN);
    registry.register_gauge(&DISPATCH_QUEUE_OLDEST_WAIT_SECONDS_AUTOMATION);
    registry.register_gauge(&DISPATCH_QUEUE_OLDEST_WAIT_SECONDS_REVIEW);
    registry.register_counter(&DISPATCH_COMPLETED);
    registry.register_gauge(&DISPATCH_DRAIN_PASS_DURATION_MS);
}

/// Per-pool depth + oldest-ready-age, computed once up front by the
/// drain loop from the same `ready` execution snapshot it dispatches
/// against, then pushed into the registry via [`record_queue_snapshot`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolQueueSample {
    pub depth: i64,
    pub oldest_wait_secs: i64,
}

/// Snapshot of all three pools' queue state for one drain pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub main: PoolQueueSample,
    pub automation: PoolQueueSample,
    pub review: PoolQueueSample,
}

/// Push a [`QueueSnapshot`] into the registry's gauges.
pub fn record_queue_snapshot(registry: &Registry, snapshot: QueueSnapshot) {
    DISPATCH_QUEUE_DEPTH_MAIN.set(registry, snapshot.main.depth);
    DISPATCH_QUEUE_OLDEST_WAIT_SECONDS_MAIN.set(registry, snapshot.main.oldest_wait_secs);
    DISPATCH_QUEUE_DEPTH_AUTOMATION.set(registry, snapshot.automation.depth);
    DISPATCH_QUEUE_OLDEST_WAIT_SECONDS_AUTOMATION.set(registry, snapshot.automation.oldest_wait_secs);
    DISPATCH_QUEUE_DEPTH_REVIEW.set(registry, snapshot.review.depth);
    DISPATCH_QUEUE_OLDEST_WAIT_SECONDS_REVIEW.set(registry, snapshot.review.oldest_wait_secs);
}

/// Record one successfully completed dispatch (a worker slot claimed).
pub fn record_dispatch_completed(registry: &Registry) {
    DISPATCH_COMPLETED.inc(registry);
}

/// Record the wall-clock duration of one `drain_ready_queue` pass.
pub fn record_drain_pass_duration_ms(registry: &Registry, duration_ms: i64) {
    DISPATCH_DRAIN_PASS_DURATION_MS.set(registry, duration_ms);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_queue_snapshot_sets_all_six_gauges() {
        let registry = Registry::new();
        register_metrics(&registry);

        record_queue_snapshot(
            &registry,
            QueueSnapshot {
                main: PoolQueueSample {
                    depth: 3,
                    oldest_wait_secs: 120,
                },
                automation: PoolQueueSample {
                    depth: 1,
                    oldest_wait_secs: 45,
                },
                review: PoolQueueSample {
                    depth: 0,
                    oldest_wait_secs: 0,
                },
            },
        );

        assert_eq!(registry.gauge_value("dispatch.queue_depth.main"), Some(3));
        assert_eq!(
            registry.gauge_value("dispatch.queue_oldest_wait_seconds.main"),
            Some(120)
        );
        assert_eq!(registry.gauge_value("dispatch.queue_depth.automation"), Some(1));
        assert_eq!(
            registry.gauge_value("dispatch.queue_oldest_wait_seconds.automation"),
            Some(45)
        );
        assert_eq!(registry.gauge_value("dispatch.queue_depth.review"), Some(0));
        assert_eq!(
            registry.gauge_value("dispatch.queue_oldest_wait_seconds.review"),
            Some(0)
        );
    }

    #[test]
    fn record_dispatch_completed_increments_counter() {
        let registry = Registry::new();
        register_metrics(&registry);
        record_dispatch_completed(&registry);
        record_dispatch_completed(&registry);
        assert_eq!(registry.counter_value("dispatch.completed"), Some(2));
    }

    #[test]
    fn record_drain_pass_duration_ms_overwrites_gauge() {
        let registry = Registry::new();
        register_metrics(&registry);
        record_drain_pass_duration_ms(&registry, 42);
        assert_eq!(registry.gauge_value("dispatch.drain_pass_duration_ms"), Some(42));
        record_drain_pass_duration_ms(&registry, 7);
        assert_eq!(registry.gauge_value("dispatch.drain_pass_duration_ms"), Some(7));
    }
}
