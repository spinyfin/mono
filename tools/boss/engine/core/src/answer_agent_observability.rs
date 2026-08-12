//! Metrics for the question → answer-agent lifecycle.
//!
//! The metrics registry intentionally has counters rather than native labelled
//! histograms. Error/reclassification labels and wait-time buckets therefore
//! use bounded dynamic counter families, while the base names remain registered
//! totals visible in every metrics listing.

use crate::merge_poller::sanitize_metric_name_component;
use crate::metrics::Registry;

crate::register_counter!(
    ENQUEUED,
    "answer_agent.enqueued",
    "Question-classified comments whose answer-agent execution was enqueued."
);
crate::register_counter!(
    STARTED,
    "answer_agent.started",
    "Answer-agent executions that started after dispatch."
);
crate::register_counter!(
    REPLIED,
    "answer_agent.replied",
    "Answer-agent runs that posted a reply."
);
crate::register_counter!(
    FAILED,
    "answer_agent.failed",
    "Answer-agent runs that failed; answer_agent.failed.<error_kind> is the bounded error-kind breakdown."
);
crate::register_counter!(
    SUPERSEDED,
    "answer_agent.superseded",
    "Answer-agent runs superseded by comment reclassification; answer_agent.superseded.<reclassified> is the reason breakdown."
);
crate::register_counter!(
    QUEUE_WAIT_MS,
    "answer_agent.queue_wait_ms",
    "Answer-agent queue-wait histogram sample count; answer_agent.queue_wait_ms.<bucket> holds bounded millisecond buckets."
);

/// Register the answer-agent lifecycle metric family during engine boot.
pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&ENQUEUED);
    registry.register_counter(&STARTED);
    registry.register_counter(&REPLIED);
    registry.register_counter(&FAILED);
    registry.register_counter(&SUPERSEDED);
    registry.register_counter(&QUEUE_WAIT_MS);
}

pub fn record_enqueued(registry: &Registry) {
    ENQUEUED.inc(registry);
}

pub fn record_started(registry: &Registry, queue_wait_ms: u64) {
    STARTED.inc(registry);
    QUEUE_WAIT_MS.inc(registry);
    registry.counter_inc_by_dynamic(
        &format!("answer_agent.queue_wait_ms.{}", queue_wait_bucket(queue_wait_ms)),
        "Answer-agent queue wait observations in this bounded millisecond bucket.",
        1,
    );
}

pub fn record_replied(registry: &Registry) {
    REPLIED.inc(registry);
}

pub fn record_failed(registry: &Registry, error_kind: &str) {
    FAILED.inc(registry);
    registry.counter_inc_by_dynamic(
        &format!("answer_agent.failed.{}", sanitize_metric_name_component(error_kind)),
        "Answer-agent failures for this error kind.",
        1,
    );
}

pub fn record_superseded(registry: &Registry, reclassified: &str) {
    SUPERSEDED.inc(registry);
    registry.counter_inc_by_dynamic(
        &format!(
            "answer_agent.superseded.{}",
            sanitize_metric_name_component(reclassified)
        ),
        "Answer-agent runs superseded for this reclassification reason.",
        1,
    );
}

fn queue_wait_bucket(queue_wait_ms: u64) -> &'static str {
    match queue_wait_ms {
        0..=999 => "lt_1s",
        1_000..=9_999 => "1s_10s",
        10_000..=59_999 => "10s_1m",
        60_000..=299_999 => "1m_5m",
        300_000..=1_799_999 => "5m_30m",
        1_800_000..=3_599_999 => "30m_1h",
        _ => "gte_1h",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_counters_and_labelled_buckets_are_incremented() {
        let registry = Registry::new();
        register_metrics(&registry);

        record_enqueued(&registry);
        record_started(&registry, 65_000);
        record_replied(&registry);
        record_failed(&registry, "no reply posted");
        record_superseded(&registry, "reclassified");

        for expected in [
            "answer_agent.enqueued",
            "answer_agent.started",
            "answer_agent.replied",
            "answer_agent.failed",
            "answer_agent.failed.no_reply_posted",
            "answer_agent.superseded",
            "answer_agent.superseded.reclassified",
            "answer_agent.queue_wait_ms",
            "answer_agent.queue_wait_ms.1m_5m",
        ] {
            assert_eq!(
                registry.counter_snapshot_one(expected).map(|snapshot| snapshot.value),
                Some(1),
                "expected {expected} to be incremented",
            );
        }
    }
}
