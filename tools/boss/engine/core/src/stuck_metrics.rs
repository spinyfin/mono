//! Daily stuck-work measurements: one persisted report per UTC day, exported
//! as gauges. Runs independently of any progression dispatch; the two
//! progression buckets are read from the same report as every other.
use crate::metrics::Registry;
use crate::work::WorkDb;
use boss_protocol::{StuckBucket, StuckReport};
use std::sync::Arc;
use std::time::Duration;

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
    STUCK_PROGRESSION_IN_FLIGHT,
    "task_stuck.progression_in_flight",
    "Daily count of items a progression agent is handling or has just handled."
);
crate::register_gauge!(
    STUCK_PROGRESSION_FAILING,
    "task_stuck.progression_failing",
    "Daily count of items progression could not unstick."
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

crate::register_gauge!(
    STUCK_PARKED,
    "task_stuck.parked",
    "Daily count of explicitly parked items."
);
crate::register_gauge!(
    STUCK_PARKED_UPSTREAM,
    "task_stuck.parked_upstream",
    "Daily count of items waiting on parked prerequisites."
);

/// Tick hourly rather than daily: a 24h interval counts monotonic time, which
/// stops while macOS sleeps and could skip whole UTC days. The per-day write
/// is idempotent, so extra ticks are cheap.
const TICK: Duration = Duration::from_secs(3600);

pub fn register_metrics(registry: &Registry) {
    for gauge in [
        &STUCK_HUMAN,
        &STUCK_CAPACITY,
        &STUCK_DEPENDENCY,
        &STUCK_PROGRESSION_IN_FLIGHT,
        &STUCK_PROGRESSION_FAILING,
        &STUCK_ENGINE,
        &STUCK_MAX_AGE,
        &STUCK_PARKED,
        &STUCK_PARKED_UPSTREAM,
    ] {
        registry.register_gauge(gauge);
    }
}

pub fn spawn<F, Fut>(registry: Arc<Registry>, db: Arc<WorkDb>, refreshed: F) -> tokio::task::JoinHandle<()>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    boss_task_scope::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let db = Arc::clone(&db);
            let sampled =
                tokio::task::spawn_blocking(move || sample(&db, boss_engine_utils::epoch_time::now_epoch_secs())).await;
            match sampled {
                Ok(Ok(report)) => {
                    record(&registry, &report);
                    refreshed().await;
                }
                Ok(Err(error)) => tracing::warn!(?error, "daily stuck snapshot failed"),
                Err(error) => tracing::warn!(?error, "daily stuck snapshot task failed"),
            }
        }
    })
}

fn sample(db: &WorkDb, now: i64) -> anyhow::Result<StuckReport> {
    let report = db.stuck_daily_snapshot(now, &|pid| crate::durable_liveness::probe_recorded_pid(pid).is_alive())?;
    if let Err(error) = db.calibrate_progression(now) {
        tracing::warn!(?error, "daily progression calibration failed");
    }
    Ok(report)
}

pub(crate) fn record(registry: &Registry, report: &StuckReport) {
    for (bucket, gauge) in [
        (StuckBucket::WaitingOnHuman, &STUCK_HUMAN),
        (StuckBucket::WaitingOnCapacity, &STUCK_CAPACITY),
        (StuckBucket::WaitingOnDependency, &STUCK_DEPENDENCY),
        (StuckBucket::ProgressionInFlight, &STUCK_PROGRESSION_IN_FLIGHT),
        (StuckBucket::ProgressionFailing, &STUCK_PROGRESSION_FAILING),
        (StuckBucket::EngineStuck, &STUCK_ENGINE),
        (StuckBucket::Parked, &STUCK_PARKED),
        (StuckBucket::ParkedUpstream, &STUCK_PARKED_UPSTREAM),
    ] {
        gauge.set(
            registry,
            report.rows.iter().filter(|r| r.bucket == bucket).count() as i64,
        );
    }
    STUCK_MAX_AGE.set(
        registry,
        report
            .rows
            .iter()
            .filter(|r| r.bucket == StuckBucket::EngineStuck)
            .map(|r| r.age_in_state_secs)
            .max()
            .unwrap_or(0),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::StuckRow;

    fn row(bucket: StuckBucket, age: i64) -> StuckRow {
        StuckRow::builder()
            .work_item_id("w")
            .name("n")
            .age_in_state_secs(age)
            .age_is_lower_bound(false)
            .bucket(bucket)
            .classification("c")
            .last_ledger_basis("b")
            .build()
    }

    #[test]
    fn calibration_failure_does_not_fail_snapshot_sampling() {
        let (_dir, db) = crate::test_support::open_db();
        db.set_metadata("progression.calibration_config", "invalid json")
            .unwrap();
        let report = sample(&db, 20_000 * 86400).unwrap();
        let registry = Registry::new();
        register_metrics(&registry);
        record(&registry, &report);
    }

    #[test]
    fn record_sets_each_bucket_count_and_engine_stuck_max_age() {
        let registry = Registry::new();
        register_metrics(&registry);
        let report = StuckReport::builder()
            .generated_at_epoch_s(0)
            .minimum_age_secs(0)
            .rows(vec![
                row(StuckBucket::WaitingOnHuman, 5),
                row(StuckBucket::Parked, 100),
                row(StuckBucket::ParkedUpstream, 200),
                row(StuckBucket::EngineStuck, 40),
                row(StuckBucket::EngineStuck, 90),
                row(StuckBucket::WaitingOnDependency, 7),
                row(StuckBucket::ProgressionInFlight, 3),
                row(StuckBucket::ProgressionInFlight, 4),
                row(StuckBucket::ProgressionFailing, 8),
            ])
            .questions_filed(0)
            .unconverted_declared_blockers(0)
            .build();
        record(&registry, &report);
        assert_eq!(registry.gauge_value("task_stuck.waiting_on_human"), Some(1));
        assert_eq!(registry.gauge_value("task_stuck.waiting_on_capacity"), Some(0));
        assert_eq!(registry.gauge_value("task_stuck.waiting_on_dependency"), Some(1));
        assert_eq!(registry.gauge_value("task_stuck.progression_in_flight"), Some(2));
        assert_eq!(registry.gauge_value("task_stuck.progression_failing"), Some(1));
        assert_eq!(registry.gauge_value("task_stuck.engine_stuck"), Some(2));
        assert_eq!(registry.gauge_value("task_stuck.parked"), Some(1));
        assert_eq!(registry.gauge_value("task_stuck.parked_upstream"), Some(1));
        assert_eq!(
            registry.gauge_value("task_stuck.engine_stuck_max_age_seconds"),
            Some(90)
        );
    }
}
