//! `WorkDb` as the engine's durable [`MetricsStore`].
//!
//! The metrics framework ([`boss_metrics`]) persists through the
//! [`MetricsStore`] trait rather than against a concrete database,
//! which is what lets it live in its own crate. This adapter is the
//! engine's side of that boundary: it forwards to the inherent
//! `WorkDb::metrics_*` methods, which own the actual SQL against the
//! `metrics_counter` / `metrics_gauge` tables in `state.db` (see
//! `work/metrics_db.rs`).

use anyhow::Result;
use boss_metrics::{MetricsCounterRow, MetricsGaugeRow, MetricsStore};

use crate::work::WorkDb;

impl MetricsStore for WorkDb {
    fn metrics_load_all(&self) -> Result<(Vec<MetricsCounterRow>, Vec<MetricsGaugeRow>)> {
        WorkDb::metrics_load_all(self)
    }

    fn metrics_flush(&self, counters: &[MetricsCounterRow], gauges: &[MetricsGaugeRow]) -> Result<()> {
        WorkDb::metrics_flush(self, counters, gauges)
    }

    fn metrics_reset_one(&self, name: &str, now_ms: i64) -> Result<(bool, bool)> {
        WorkDb::metrics_reset_one(self, name, now_ms)
    }

    fn metrics_reset_all(&self, now_ms: i64) -> Result<(usize, usize)> {
        WorkDb::metrics_reset_all(self, now_ms)
    }
}

/// Round-trip coverage for the real sqlite-backed store. The
/// framework's own logic is unit-tested against an in-memory fake
/// inside `boss_metrics`; these tests exist to pin the behaviour of
/// the actual `state.db` implementation behind the trait — notably
/// the `u64`-as-`i64` bit round-trip for monotonic counters, which a
/// fake can't exercise.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::register_counter;
    use crate::register_gauge;
    use boss_metrics::{Registry, flush_all, seed_from_db};
    use std::path::PathBuf;

    register_counter!(
        TEST_PERSIST_COUNTER,
        "test_persist.counter",
        "Counter used by persistence round-trip tests."
    );
    register_gauge!(
        TEST_PERSIST_GAUGE,
        "test_persist.gauge",
        "Gauge used by persistence round-trip tests."
    );

    fn open_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory work db")
    }

    #[test]
    fn round_trip_counter_value_across_simulated_restart() {
        let db = open_db();

        // First "engine boot": register, increment, flush.
        let registry_one = Registry::new();
        registry_one.register_counter(&TEST_PERSIST_COUNTER);
        TEST_PERSIST_COUNTER.inc_by(&registry_one, 5);
        flush_all(&registry_one, &db).expect("flush 1");
        drop(registry_one);

        // Second "engine boot": fresh registry, seed from db, value
        // must come back as 5.
        let registry_two = Registry::new();
        registry_two.register_counter(&TEST_PERSIST_COUNTER);
        seed_from_db(&registry_two, &db).expect("seed");
        assert_eq!(registry_two.counter_value("test_persist.counter"), Some(5));

        // Additional increments accumulate on top of the seeded
        // value.
        TEST_PERSIST_COUNTER.inc_by(&registry_two, 7);
        assert_eq!(registry_two.counter_value("test_persist.counter"), Some(12));

        flush_all(&registry_two, &db).expect("flush 2");
        let (counters, _) = MetricsStore::metrics_load_all(&db).expect("load");
        let row = counters
            .iter()
            .find(|r| r.name == "test_persist.counter")
            .expect("counter row persisted");
        assert_eq!(row.value, 12);
        assert_eq!(row.description, "Counter used by persistence round-trip tests.");
    }

    /// Counters are `u64` in memory but sqlite only has signed
    /// 64-bit integers, so the store round-trips them as raw bits. A
    /// value above `i64::MAX` must survive the encode/decode.
    #[test]
    fn counter_value_above_i64_max_survives_the_round_trip() {
        let db = open_db();
        let huge = u64::MAX - 3;

        let registry_one = Registry::new();
        registry_one.register_counter(&TEST_PERSIST_COUNTER);
        TEST_PERSIST_COUNTER.inc_by(&registry_one, huge);
        flush_all(&registry_one, &db).expect("flush");

        let registry_two = Registry::new();
        registry_two.register_counter(&TEST_PERSIST_COUNTER);
        seed_from_db(&registry_two, &db).expect("seed");
        assert_eq!(
            registry_two.counter_value("test_persist.counter"),
            Some(huge),
            "counters above i64::MAX must round-trip through sqlite unchanged",
        );
    }

    #[test]
    fn round_trip_gauge_value_across_simulated_restart() {
        let db = open_db();

        let registry_one = Registry::new();
        registry_one.register_gauge(&TEST_PERSIST_GAUGE);
        TEST_PERSIST_GAUGE.set(&registry_one, 999);
        flush_all(&registry_one, &db).expect("flush 1");

        let registry_two = Registry::new();
        registry_two.register_gauge(&TEST_PERSIST_GAUGE);
        seed_from_db(&registry_two, &db).expect("seed");
        assert_eq!(registry_two.gauge_value("test_persist.gauge"), Some(999));
    }

    #[test]
    fn unknown_persisted_row_is_kept_as_stale_not_dropped() {
        let db = open_db();

        // Simulate a previous engine version's counter that no
        // longer matches any registered handle.
        let registry_one = Registry::new();
        static OLD_HANDLE: boss_metrics::CounterHandle = boss_metrics::CounterHandle::new(
            "test_persist.removed_counter",
            "an old counter from a previous engine version",
        );
        registry_one.register_counter(&OLD_HANDLE);
        OLD_HANDLE.inc_by(&registry_one, 42);
        flush_all(&registry_one, &db).expect("flush 1");

        // New engine boot: no register_counter call for the old
        // name. The row must come back as stale.
        let registry_two = Registry::new();
        seed_from_db(&registry_two, &db).expect("seed");
        let snaps = registry_two.counter_snapshots();
        let stale = snaps
            .iter()
            .find(|s| s.name == "test_persist.removed_counter")
            .expect("stale row should be retained");
        assert!(stale.stale, "rehydrated unknown counter should be marked stale");
        assert_eq!(stale.value, 42);

        // And subsequent flushes must not drop it from the table —
        // the row stays.
        flush_all(&registry_two, &db).expect("flush 2");
        let (counters, _) = MetricsStore::metrics_load_all(&db).expect("load");
        assert!(
            counters.iter().any(|r| r.name == "test_persist.removed_counter"),
            "stale row must survive subsequent flushes",
        );
    }

    #[test]
    fn flush_is_a_no_op_when_registry_is_empty() {
        let db = open_db();
        let registry = Registry::new();
        flush_all(&registry, &db).expect("flush no-op");
        let (counters, gauges) = MetricsStore::metrics_load_all(&db).expect("load");
        assert!(counters.is_empty());
        assert!(gauges.is_empty());
    }

    #[test]
    fn metrics_reset_one_zeros_counter_row_in_db() {
        let db = open_db();
        let registry = Registry::new();
        registry.register_counter(&TEST_PERSIST_COUNTER);
        TEST_PERSIST_COUNTER.inc_by(&registry, 20);
        flush_all(&registry, &db).expect("flush");

        db.metrics_reset_one("test_persist.counter", 9999).expect("reset one");
        let (counters, _) = MetricsStore::metrics_load_all(&db).expect("load");
        let row = counters.iter().find(|r| r.name == "test_persist.counter").unwrap();
        assert_eq!(row.value, 0);
        assert_eq!(row.updated_at_ms, 9999);
    }

    #[test]
    fn metrics_reset_one_returns_false_for_unknown_name() {
        let db = open_db();
        let (c, g) = db.metrics_reset_one("does.not.exist", 1234).expect("reset");
        assert!(!c);
        assert!(!g);
    }

    #[test]
    fn metrics_reset_all_zeros_every_row() {
        let db = open_db();
        let registry = Registry::new();
        registry.register_counter(&TEST_PERSIST_COUNTER);
        registry.register_gauge(&TEST_PERSIST_GAUGE);
        TEST_PERSIST_COUNTER.inc_by(&registry, 5);
        TEST_PERSIST_GAUGE.set(&registry, 77);
        flush_all(&registry, &db).expect("flush");

        let (counter_count, gauge_count) = db.metrics_reset_all(8888).expect("reset all");
        assert_eq!(counter_count, 1);
        assert_eq!(gauge_count, 1);

        let (counters, gauges) = MetricsStore::metrics_load_all(&db).expect("load");
        assert_eq!(counters[0].value, 0);
        assert_eq!(gauges[0].value, 0);
    }
}
