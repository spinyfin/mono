//! Daily measurements begin immediately, independently of progression dispatch.
use crate::work::WorkDb;
use boss_metrics::Registry;
use std::sync::Arc;

crate::register_gauge!(
    STUCK_HUMAN,
    "task_stuck.waiting_on_human",
    "Daily count of items waiting on a human."
);
crate::register_gauge!(
    STUCK_CAPACITY,
    "task_stuck.waiting_on_capacity",
    "Daily count of items waiting on capacity."
);
crate::register_gauge!(
    STUCK_DEPENDENCY,
    "task_stuck.waiting_on_dependency",
    "Daily count of items waiting on a progressing prerequisite."
);
crate::register_gauge!(
    STUCK_ENGINE,
    "task_stuck.engine_stuck",
    "Daily count of items with no work or legitimate hold."
);
crate::register_gauge!(
    STUCK_MAX_AGE,
    "task_stuck.engine_stuck_max_age_seconds",
    "Daily maximum engine-stuck state age in seconds."
);

pub fn register_metrics(registry: &Registry) {
    for gauge in [
        &STUCK_HUMAN,
        &STUCK_CAPACITY,
        &STUCK_DEPENDENCY,
        &STUCK_ENGINE,
        &STUCK_MAX_AGE,
    ] {
        registry.register_gauge(gauge);
    }
}

pub fn spawn(registry: Arc<Registry>, db: Arc<WorkDb>) -> tokio::task::JoinHandle<()> {
    boss_task_scope::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let now = boss_engine_utils::epoch_time::now_epoch_secs();
            match db.stuck_daily_snapshot(now) {
                Ok(report) => record(&registry, &report),
                Err(error) => tracing::warn!(?error, "daily stuck snapshot failed"),
            }
        }
    })
}

pub(crate) fn record(registry: &Registry, report: &boss_protocol::StuckReport) {
    for (bucket, gauge) in [
        ("waiting_on_human", &STUCK_HUMAN),
        ("waiting_on_capacity", &STUCK_CAPACITY),
        ("waiting_on_dependency", &STUCK_DEPENDENCY),
        ("engine_stuck", &STUCK_ENGINE),
    ] {
        gauge.set(
            registry,
            report.rows.iter().filter(|r| r.bucket.as_str() == bucket).count() as i64,
        );
    }
    STUCK_MAX_AGE.set(
        registry,
        report
            .rows
            .iter()
            .filter(|r| r.bucket == boss_protocol::StuckBucket::EngineStuck)
            .map(|r| r.age_in_state_secs)
            .max()
            .unwrap_or(0),
    );
}
