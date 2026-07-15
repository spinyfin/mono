//! The storage boundary for persisted metrics.
//!
//! [`MetricsStore`] is the only thing this crate knows about
//! durable storage: the flush task and the startup rehydrate call
//! through it rather than against a concrete database. The engine
//! implements it for its `WorkDb` (backed by the `metrics_counter` /
//! `metrics_gauge` tables in `state.db`), which is what keeps the
//! dependency edge one-directional — `boss_engine` -> `boss_metrics`,
//! never the reverse.

use std::sync::Arc;

use anyhow::Result;

/// One row pulled from the counter table. The framework rehydrates
/// these into the in-memory registry on engine start so monotonic
/// totals span restarts.
#[derive(Debug, Clone)]
pub struct MetricsCounterRow {
    pub name: String,
    pub value: u64,
    pub updated_at_ms: i64,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct MetricsGaugeRow {
    pub name: String,
    pub value: i64,
    pub observed_at_ms: i64,
    pub description: String,
}

/// Durable storage for counter / gauge rows.
///
/// Implementors are expected to be cheap to share across threads —
/// [`crate::spawn_flush_task`] holds one for the engine's lifetime.
pub trait MetricsStore: Send + Sync {
    /// Load every persisted counter and gauge row for the startup
    /// rehydrate. Order is unspecified — [`crate::seed_from_db`],
    /// the only caller, doesn't care.
    fn metrics_load_all(&self) -> Result<(Vec<MetricsCounterRow>, Vec<MetricsGaugeRow>)>;

    /// UPSERT every counter and gauge snapshot, atomically if the
    /// backing store supports it. Called on the flush cadence and
    /// once more from the graceful-shutdown path.
    fn metrics_flush(&self, counters: &[MetricsCounterRow], gauges: &[MetricsGaugeRow]) -> Result<()>;

    /// Zero one metric (counter or gauge). Returns
    /// `(counter_cleared, gauge_cleared)` so the caller can tell the
    /// operator which kind was found.
    fn metrics_reset_one(&self, name: &str, now_ms: i64) -> Result<(bool, bool)>;

    /// Zero every counter and gauge row. Returns
    /// `(counters_cleared, gauges_cleared)`.
    fn metrics_reset_all(&self, now_ms: i64) -> Result<(usize, usize)>;
}

/// Forward through `Arc`, so a caller holding the shared handle it
/// already keeps around (the engine owns an `Arc<WorkDb>`) can pass
/// it straight to [`crate::flush_all`] / [`crate::seed_from_db`]
/// without unwrapping it first. Generic parameters don't auto-deref
/// the way the old concrete `&WorkDb` signature did, so without this
/// every call site would need a `&*`.
impl<T: MetricsStore + ?Sized> MetricsStore for Arc<T> {
    fn metrics_load_all(&self) -> Result<(Vec<MetricsCounterRow>, Vec<MetricsGaugeRow>)> {
        (**self).metrics_load_all()
    }

    fn metrics_flush(&self, counters: &[MetricsCounterRow], gauges: &[MetricsGaugeRow]) -> Result<()> {
        (**self).metrics_flush(counters, gauges)
    }

    fn metrics_reset_one(&self, name: &str, now_ms: i64) -> Result<(bool, bool)> {
        (**self).metrics_reset_one(name, now_ms)
    }

    fn metrics_reset_all(&self, now_ms: i64) -> Result<(usize, usize)> {
        (**self).metrics_reset_all(now_ms)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory [`MetricsStore`] for this crate's own tests, with
    /// the same upsert-by-name semantics as the engine's sqlite
    /// implementation. The sqlite behaviour itself (including the
    /// `u64`-as-`i64` bit round-trip) is covered against the real
    /// `WorkDb` over in `boss_engine::metrics_store`.
    #[derive(Default)]
    pub(crate) struct FakeStore {
        counters: Mutex<HashMap<String, MetricsCounterRow>>,
        gauges: Mutex<HashMap<String, MetricsGaugeRow>>,
    }

    impl MetricsStore for FakeStore {
        fn metrics_load_all(&self) -> Result<(Vec<MetricsCounterRow>, Vec<MetricsGaugeRow>)> {
            let counters = self.counters.lock().unwrap().values().cloned().collect();
            let gauges = self.gauges.lock().unwrap().values().cloned().collect();
            Ok((counters, gauges))
        }

        fn metrics_flush(&self, counters: &[MetricsCounterRow], gauges: &[MetricsGaugeRow]) -> Result<()> {
            let mut counter_map = self.counters.lock().unwrap();
            for row in counters {
                counter_map.insert(row.name.clone(), row.clone());
            }
            let mut gauge_map = self.gauges.lock().unwrap();
            for row in gauges {
                gauge_map.insert(row.name.clone(), row.clone());
            }
            Ok(())
        }

        fn metrics_reset_one(&self, name: &str, now_ms: i64) -> Result<(bool, bool)> {
            let mut counter_cleared = false;
            if let Some(row) = self.counters.lock().unwrap().get_mut(name) {
                row.value = 0;
                row.updated_at_ms = now_ms;
                counter_cleared = true;
            }
            let mut gauge_cleared = false;
            if let Some(row) = self.gauges.lock().unwrap().get_mut(name) {
                row.value = 0;
                row.observed_at_ms = now_ms;
                gauge_cleared = true;
            }
            Ok((counter_cleared, gauge_cleared))
        }

        fn metrics_reset_all(&self, now_ms: i64) -> Result<(usize, usize)> {
            let mut counter_map = self.counters.lock().unwrap();
            for row in counter_map.values_mut() {
                row.value = 0;
                row.updated_at_ms = now_ms;
            }
            let mut gauge_map = self.gauges.lock().unwrap();
            for row in gauge_map.values_mut() {
                row.value = 0;
                row.observed_at_ms = now_ms;
            }
            Ok((counter_map.len(), gauge_map.len()))
        }
    }
}
