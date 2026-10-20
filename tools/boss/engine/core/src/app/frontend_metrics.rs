//! Frontend traffic totals and bounded per-variant breakdowns.
//!
//! Labels use lowercase Rust variant names in dotted components, matching
//! github_api_usage's dynamic registration pattern. No request ids,
//! topic strings, or payload fields participate in a metric name.
use crate::merge_poller::sanitize_metric_name_component;
use boss_metrics::Registry;

use super::session_queue::EnqueueOutcome;

crate::register_counter!(
    REQUESTS,
    "frontend_requests_total",
    "Decoded frontend requests before policy checks and lane routing."
);
crate::register_counter!(
    PUSHES,
    "frontend_pushes_total",
    "Uncorrelated frontend push enqueue attempts, including rejected attempts."
);

pub(crate) fn register_metrics(registry: &Registry) {
    registry.register_counter(&REQUESTS);
    registry.register_counter(&PUSHES);
}

pub(super) fn record_request(registry: &Registry, verb: &str) {
    let verb = sanitize_metric_name_component(verb);
    REQUESTS.inc(registry);
    registry.counter_inc_by_dynamic(
        &format!("frontend_requests_total.{verb}"),
        "Decoded frontend requests of this enum variant, before lane routing.",
        1,
    );
}

pub(super) fn record_push(registry: &Registry, kind: &str, outcome: &EnqueueOutcome) {
    let kind = sanitize_metric_name_component(kind);
    let outcome = match outcome {
        EnqueueOutcome::Enqueued => "enqueued",
        EnqueueOutcome::Coalesced => "coalesced",
        EnqueueOutcome::Degraded => "degraded",
        EnqueueOutcome::Slow => "slow",
        EnqueueOutcome::Closed => "closed",
    };
    PUSHES.inc(registry);
    registry.counter_inc_by_dynamic(
        &format!("frontend_pushes_total.{kind}.{outcome}"),
        "Uncorrelated frontend push enqueue attempts by enum variant and queue outcome.",
        1,
    );
}

#[cfg(test)]
pub(super) fn test_registry() -> std::sync::Arc<Registry> {
    let registry = std::sync::Arc::new(Registry::new());
    register_metrics(&registry);
    registry
}
