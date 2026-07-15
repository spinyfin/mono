//! Persistence bridge between the in-memory [`Registry`] and a
//! durable [`MetricsStore`] (the engine backs this with the
//! `metrics_counter` / `metrics_gauge` tables in `state.db`).
//!
//! - [`seed_from_db`] is called once on engine startup, after every
//!   handle has been registered.
//! - [`spawn_flush_task`] runs every 30 seconds and upserts every
//!   registered counter / gauge snapshot in a single transaction.
//! - [`flush_all`] is called from the graceful-shutdown path so the
//!   last 0–30 s of increments survive a normal exit. Crash-loss is
//!   bounded to the flush interval — acceptable for monotonic counts
//!   (see design §"Persistence: state.db table").

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::task::JoinHandle;

use crate::store::{MetricsCounterRow, MetricsGaugeRow, MetricsStore};

use super::registry::{Registry, now_ms};

/// How often the periodic flush task wakes up and snapshots the
/// registry into the store. Picked for the
/// "did the reconstruction path fire?" use case: a 30 s window means
/// at most ~30 s of increments are lost on crash, and the cost is
/// one transaction every 30 s — negligible against the engine's
/// existing write traffic.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// Read every persisted counter / gauge row and seed the in-memory
/// registry. Rows whose name matches a registered handle update
/// that handle's value in place; rows without a matching handle are
/// inserted as "stale" so a future `bossctl metrics list` can still
/// see them (design §"Risks / open questions" item 3).
///
/// Call after every handle is registered so the rehydrate knows what
/// counts as stale.
pub fn seed_from_db<S: MetricsStore + ?Sized>(registry: &Registry, store: &S) -> Result<()> {
    let (counters, gauges) = store.metrics_load_all()?;
    for row in counters {
        if !registry.seed_counter(&row.name, row.value, row.updated_at_ms) {
            registry.insert_stale_counter(&row.name, &row.description, row.value, row.updated_at_ms);
        }
    }
    for row in gauges {
        if !registry.seed_gauge(&row.name, row.value, row.observed_at_ms) {
            registry.insert_stale_gauge(&row.name, &row.description, row.value, row.observed_at_ms);
        }
    }
    Ok(())
}

/// Spawn the periodic flush task on the current tokio runtime. The
/// returned `JoinHandle` is held by `ServerState` until shutdown so
/// the task is bound to the engine's lifetime.
pub fn spawn_flush_task<S: MetricsStore + 'static>(registry: Arc<Registry>, store: Arc<S>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        // `Skip` keeps the task from firing many catch-up flushes
        // after a stall (e.g. machine sleep) — a single fresh flush
        // is what we want on resume.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick fires immediately; skip it so we don't
        // double-flush right after startup (seed-from-db already
        // ran).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(err) = flush_all(&registry, &*store) {
                tracing::warn!(?err, "metrics flush failed; will retry on next tick");
            }
        }
    })
}

/// Snapshot every registered counter / gauge and upsert into the
/// store. Stale rows (rehydrated rows whose name no longer matches a
/// registered handle) are skipped so we don't rewrite them on every
/// flush; the persisted row stays untouched.
pub fn flush_all<S: MetricsStore + ?Sized>(registry: &Registry, store: &S) -> Result<()> {
    let counter_snaps = registry.counter_snapshots();
    let gauge_snaps = registry.gauge_snapshots();
    let now = now_ms();

    let counters: Vec<MetricsCounterRow> = counter_snaps
        .into_iter()
        .filter(|s| !s.stale)
        .map(|s| MetricsCounterRow {
            name: s.name,
            value: s.value,
            // Use the in-memory `updated_at_ms` so the persisted
            // timestamp tracks the most recent increment, not the
            // wall-clock at flush time. Falls back to `now` if the
            // counter has never been touched since seed.
            updated_at_ms: if s.updated_at_ms == 0 { now } else { s.updated_at_ms },
            description: s.description,
        })
        .collect();

    let gauges: Vec<MetricsGaugeRow> = gauge_snaps
        .into_iter()
        .filter(|s| !s.stale)
        .map(|s| MetricsGaugeRow {
            name: s.name,
            value: s.value,
            observed_at_ms: if s.observed_at_ms == 0 { now } else { s.observed_at_ms },
            description: s.description,
        })
        .collect();

    store.metrics_flush(&counters, &gauges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::CounterHandle;
    use crate::store::testing::FakeStore;
    use crate::{register_counter, register_gauge};

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

    #[test]
    fn round_trip_counter_value_across_simulated_restart() {
        let store = FakeStore::default();

        // First "engine boot": register, increment, flush.
        let registry_one = Registry::new();
        registry_one.register_counter(&TEST_PERSIST_COUNTER);
        TEST_PERSIST_COUNTER.inc_by(&registry_one, 5);
        flush_all(&registry_one, &store).expect("flush 1");
        drop(registry_one);

        // Second "engine boot": fresh registry, seed from the store,
        // value must come back as 5.
        let registry_two = Registry::new();
        registry_two.register_counter(&TEST_PERSIST_COUNTER);
        seed_from_db(&registry_two, &store).expect("seed");
        assert_eq!(registry_two.counter_value("test_persist.counter"), Some(5));

        // Additional increments accumulate on top of the seeded
        // value.
        TEST_PERSIST_COUNTER.inc_by(&registry_two, 7);
        assert_eq!(registry_two.counter_value("test_persist.counter"), Some(12));

        flush_all(&registry_two, &store).expect("flush 2");
        let (counters, _) = store.metrics_load_all().expect("load");
        let row = counters
            .iter()
            .find(|r| r.name == "test_persist.counter")
            .expect("counter row persisted");
        assert_eq!(row.value, 12);
        assert_eq!(row.description, "Counter used by persistence round-trip tests.");
    }

    #[test]
    fn round_trip_gauge_value_across_simulated_restart() {
        let store = FakeStore::default();

        let registry_one = Registry::new();
        registry_one.register_gauge(&TEST_PERSIST_GAUGE);
        TEST_PERSIST_GAUGE.set(&registry_one, 999);
        flush_all(&registry_one, &store).expect("flush 1");

        let registry_two = Registry::new();
        registry_two.register_gauge(&TEST_PERSIST_GAUGE);
        seed_from_db(&registry_two, &store).expect("seed");
        assert_eq!(registry_two.gauge_value("test_persist.gauge"), Some(999));
    }

    #[test]
    fn unknown_persisted_row_is_kept_as_stale_not_dropped() {
        let store = FakeStore::default();

        // Simulate a previous engine version's counter that no
        // longer matches any registered handle.
        let registry_one = Registry::new();
        static OLD_HANDLE: CounterHandle = CounterHandle::new(
            "test_persist.removed_counter",
            "an old counter from a previous engine version",
        );
        registry_one.register_counter(&OLD_HANDLE);
        OLD_HANDLE.inc_by(&registry_one, 42);
        flush_all(&registry_one, &store).expect("flush 1");

        // New engine boot: no register_counter call for the old
        // name. The row must come back as stale.
        let registry_two = Registry::new();
        seed_from_db(&registry_two, &store).expect("seed");
        let snaps = registry_two.counter_snapshots();
        let stale = snaps
            .iter()
            .find(|s| s.name == "test_persist.removed_counter")
            .expect("stale row should be retained");
        assert!(stale.stale, "rehydrated unknown counter should be marked stale");
        assert_eq!(stale.value, 42);

        // And subsequent flushes must not drop it from the store —
        // the row stays.
        flush_all(&registry_two, &store).expect("flush 2");
        let (counters, _) = store.metrics_load_all().expect("load");
        assert!(
            counters.iter().any(|r| r.name == "test_persist.removed_counter"),
            "stale row must survive subsequent flushes",
        );
    }

    #[test]
    fn stale_row_is_adopted_after_handle_is_added_back() {
        let store = FakeStore::default();

        // Persist a counter under a name first.
        let registry_one = Registry::new();
        registry_one.register_counter(&TEST_PERSIST_COUNTER);
        TEST_PERSIST_COUNTER.inc_by(&registry_one, 4);
        flush_all(&registry_one, &store).expect("flush 1");

        // Boot 2: registry is fresh, do NOT register first —
        // simulate the cold path where the rehydrate runs before
        // registration. The row is stale at first…
        let registry_two = Registry::new();
        seed_from_db(&registry_two, &store).expect("seed");
        assert!(
            registry_two
                .counter_snapshots()
                .iter()
                .find(|s| s.name == "test_persist.counter")
                .map(|s| s.stale)
                .unwrap_or(false),
            "should be stale before registration"
        );

        // …then registration adopts it without losing the value.
        registry_two.register_counter(&TEST_PERSIST_COUNTER);
        let snap = registry_two
            .counter_snapshots()
            .into_iter()
            .find(|s| s.name == "test_persist.counter")
            .expect("entry");
        assert!(!snap.stale);
        assert_eq!(snap.value, 4);
    }

    #[test]
    fn flush_is_a_no_op_when_registry_is_empty() {
        let store = FakeStore::default();
        let registry = Registry::new();
        flush_all(&registry, &store).expect("flush no-op");
        let (counters, gauges) = store.metrics_load_all().expect("load");
        assert!(counters.is_empty());
        assert!(gauges.is_empty());
    }

    /// The engine hands `flush_all` an `&Arc<WorkDb>` (and
    /// `spawn_flush_task` an owned `Arc`), so the blanket `Arc<T>`
    /// forwarding impl is load-bearing for the real call sites.
    #[test]
    fn flush_all_accepts_an_arc_wrapped_store() {
        let store = Arc::new(FakeStore::default());
        let registry = Registry::new();
        registry.register_counter(&TEST_PERSIST_COUNTER);
        TEST_PERSIST_COUNTER.inc_by(&registry, 3);

        flush_all(&registry, &store).expect("flush through Arc");

        let (counters, _) = store.metrics_load_all().expect("load");
        assert_eq!(
            counters
                .iter()
                .find(|r| r.name == "test_persist.counter")
                .map(|r| r.value),
            Some(3),
        );
    }
}
