//! Engine wiring for host-load sampling: gauges, the `state.db` series, and
//! the background task.
//!
//! The sampler itself (syscall layer, CPU-delta math, tick loop) lives in
//! [`boss_engine_host_load`] and knows nothing about the engine. This module
//! is the engine's side of that boundary:
//!
//! - registers the `host.*` gauges and publishes each sample into them, so
//!   the current values show up in `bossctl metrics list --prefix host.`
//!   and the macOS app's Metrics view;
//! - implements [`SampleStore`] for [`WorkDb`], which keeps the retained
//!   series in the `host_load_samples` table (pruned to the configured
//!   retention);
//! - reads the interval and retention from `settings.toml` and spawns the
//!   task.
//!
//! The data is collect-and-expose only: nothing in dispatch reads it.
//!
//! Gauges hold `i64`, so fractional values are fixed-point ×100 (`_x100`
//! suffix: `15550` is a load average of 155.50, and `9950` is 99.5 %). The
//! retained series keeps full `f64` precision.

use std::sync::Arc;

use anyhow::Result;
use boss_engine_host_load::{
    DEFAULT_INTERVAL_SECONDS, DEFAULT_RETENTION_DAYS, LoopConfig, SampleObserver, SampleStore, platform_probe,
    spawn_sampler,
};
use tokio::task::JoinHandle;

// `bossctl` reads the series through `WorkDb` and renders it with these.
pub use boss_engine_host_load::{HostSample, format_local_ms};

use crate::metrics::Registry;
use crate::settings::SettingsStore;
use crate::work::WorkDb;

/// Settings keys; defaults and bounds live in [`crate::settings::INT_REGISTRY`].
pub const INTERVAL_SETTING: &str = "host_load.sample_interval_seconds";
pub const RETENTION_SETTING: &str = "host_load.retention_days";

crate::register_gauge!(
    HOST_LOGICAL_CPUS,
    "host.logical_cpus",
    "Logical CPU count of the host (hw.logicalcpu)."
);
crate::register_gauge!(
    HOST_LOAD_AVG_1M_X100,
    "host.load_avg_1m_x100",
    "1-minute load average ×100 (vm.loadavg), at the latest host-load sample."
);
crate::register_gauge!(
    HOST_LOAD_AVG_5M_X100,
    "host.load_avg_5m_x100",
    "5-minute load average ×100 (vm.loadavg), at the latest host-load sample."
);
crate::register_gauge!(
    HOST_LOAD_AVG_15M_X100,
    "host.load_avg_15m_x100",
    "15-minute load average ×100 (vm.loadavg), at the latest host-load sample."
);
crate::register_gauge!(
    HOST_CPU_BUSY_PCT_X100,
    "host.cpu_busy_pct_x100",
    "Host CPU busy percentage ×100 (user+system+nice) over the last sampling interval."
);
crate::register_gauge!(
    HOST_CPU_USER_PCT_X100,
    "host.cpu_user_pct_x100",
    "Host CPU user (incl. nice) percentage ×100 over the last sampling interval."
);
crate::register_gauge!(
    HOST_CPU_SYSTEM_PCT_X100,
    "host.cpu_system_pct_x100",
    "Host CPU system percentage ×100 over the last sampling interval."
);
crate::register_gauge!(
    HOST_CPU_IDLE_PCT_X100,
    "host.cpu_idle_pct_x100",
    "Host CPU idle percentage ×100 over the last sampling interval."
);
crate::register_gauge!(
    HOST_MEM_TOTAL_BYTES,
    "host.mem_total_bytes",
    "Physical memory in bytes (hw.memsize)."
);
crate::register_gauge!(
    HOST_MEM_USED_BYTES,
    "host.mem_used_bytes",
    "Memory used in bytes: app + wired + compressed, as Activity Monitor counts it."
);
crate::register_gauge!(
    HOST_MEM_COMPRESSOR_BYTES,
    "host.mem_compressor_bytes",
    "Physical memory occupied by the compressor, in bytes."
);
crate::register_gauge!(
    HOST_MEM_FREE_BYTES,
    "host.mem_free_bytes",
    "Free-list memory in bytes (excluding speculative pages)."
);
crate::register_gauge!(
    HOST_MEM_AVAILABLE_BYTES,
    "host.mem_available_bytes",
    "Physical memory not counted as used (total minus used), in bytes."
);
crate::register_gauge!(
    HOST_MEM_PRESSURE_LEVEL_PCT,
    "host.mem_pressure_level_pct",
    "Kernel memory-pressure indicator (kern.memorystatus_level): percent of memory available, 0-100. Low means pressure."
);
crate::register_gauge!(
    HOST_SWAP_TOTAL_BYTES,
    "host.swap_total_bytes",
    "Swap space in bytes (vm.swapusage total)."
);
crate::register_gauge!(
    HOST_SWAP_USED_BYTES,
    "host.swap_used_bytes",
    "Swap in use in bytes (vm.swapusage used)."
);

/// Register every handle this module declares. Called from
/// [`crate::metrics_init::init_all`] at engine startup.
pub fn register_metrics(registry: &Registry) {
    for gauge in [
        &HOST_LOGICAL_CPUS,
        &HOST_LOAD_AVG_1M_X100,
        &HOST_LOAD_AVG_5M_X100,
        &HOST_LOAD_AVG_15M_X100,
        &HOST_CPU_BUSY_PCT_X100,
        &HOST_CPU_USER_PCT_X100,
        &HOST_CPU_SYSTEM_PCT_X100,
        &HOST_CPU_IDLE_PCT_X100,
        &HOST_MEM_TOTAL_BYTES,
        &HOST_MEM_USED_BYTES,
        &HOST_MEM_COMPRESSOR_BYTES,
        &HOST_MEM_FREE_BYTES,
        &HOST_MEM_AVAILABLE_BYTES,
        &HOST_MEM_PRESSURE_LEVEL_PCT,
        &HOST_SWAP_TOTAL_BYTES,
        &HOST_SWAP_USED_BYTES,
    ] {
        registry.register_gauge(gauge);
    }
}

fn x100(value: f64) -> i64 {
    (value * 100.0).round() as i64
}

/// Push one sample into the gauges. A field that wasn't measured leaves
/// its gauge at the last measured value rather than resetting it to zero.
pub fn publish(registry: &Registry, sample: &HostSample) {
    let set_i = |gauge: &crate::metrics::GaugeHandle, value: Option<i64>| {
        if let Some(v) = value {
            gauge.set(registry, v);
        }
    };
    let set_f = |gauge: &crate::metrics::GaugeHandle, value: Option<f64>| set_i(gauge, value.map(x100));

    set_i(&HOST_LOGICAL_CPUS, sample.logical_cpus);
    set_f(&HOST_LOAD_AVG_1M_X100, sample.load_avg_1m);
    set_f(&HOST_LOAD_AVG_5M_X100, sample.load_avg_5m);
    set_f(&HOST_LOAD_AVG_15M_X100, sample.load_avg_15m);
    set_f(&HOST_CPU_BUSY_PCT_X100, sample.cpu_busy_pct);
    set_f(&HOST_CPU_USER_PCT_X100, sample.cpu_user_pct);
    set_f(&HOST_CPU_SYSTEM_PCT_X100, sample.cpu_system_pct);
    set_f(&HOST_CPU_IDLE_PCT_X100, sample.cpu_idle_pct);
    set_i(&HOST_MEM_TOTAL_BYTES, sample.mem_total_bytes);
    set_i(&HOST_MEM_USED_BYTES, sample.mem_used_bytes);
    set_i(&HOST_MEM_COMPRESSOR_BYTES, sample.mem_compressor_bytes);
    set_i(&HOST_MEM_FREE_BYTES, sample.mem_free_bytes);
    set_i(&HOST_MEM_AVAILABLE_BYTES, sample.mem_available_bytes);
    set_i(&HOST_MEM_PRESSURE_LEVEL_PCT, sample.mem_pressure_level_pct);
    set_i(&HOST_SWAP_TOTAL_BYTES, sample.swap_total_bytes);
    set_i(&HOST_SWAP_USED_BYTES, sample.swap_used_bytes);
}

impl SampleStore for WorkDb {
    fn insert_host_load_sample(&self, sample: &HostSample) -> Result<()> {
        self.host_load_insert(sample)
    }

    fn prune_host_load_samples(&self, older_than_ms: i64) -> Result<usize> {
        self.host_load_prune(older_than_ms)
    }
}

/// Start the sampler task using the installation's settings. Interval and
/// retention are read once, here; an edited `settings.toml` takes effect
/// on the next engine start.
pub fn spawn(registry: Arc<Registry>, work_db: Arc<WorkDb>, settings: &SettingsStore) -> JoinHandle<()> {
    let interval = settings
        .get_int(INTERVAL_SETTING)
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(DEFAULT_INTERVAL_SECONDS);
    let retention = settings
        .get_int(RETENTION_SETTING)
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS);
    let config = LoopConfig::new(interval, retention);
    tracing::info!(
        interval_seconds = config.interval.as_secs(),
        retention_days = retention,
        "host_load: sampler starting",
    );
    let observer: SampleObserver = Arc::new(move |sample| publish(&registry, sample));
    spawn_sampler(platform_probe(), work_db, observer, config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_sets_fixed_point_gauges_and_leaves_unmeasured_ones_alone() {
        let registry = Registry::new();
        register_metrics(&registry);

        publish(
            &registry,
            &HostSample {
                ts_ms: 1,
                logical_cpus: Some(12),
                load_avg_1m: Some(155.456),
                cpu_busy_pct: Some(99.5),
                mem_used_bytes: Some(61 << 30),
                swap_used_bytes: Some(10 << 30),
                ..HostSample::default()
            },
        );
        assert_eq!(registry.gauge_value("host.logical_cpus"), Some(12));
        assert_eq!(registry.gauge_value("host.load_avg_1m_x100"), Some(15546));
        assert_eq!(registry.gauge_value("host.cpu_busy_pct_x100"), Some(9950));
        assert_eq!(registry.gauge_value("host.mem_used_bytes"), Some(61 << 30));

        // A later sample that failed to read memory keeps the last good value.
        publish(
            &registry,
            &HostSample {
                ts_ms: 2,
                load_avg_1m: Some(1.0),
                ..HostSample::default()
            },
        );
        assert_eq!(registry.gauge_value("host.load_avg_1m_x100"), Some(100));
        assert_eq!(registry.gauge_value("host.mem_used_bytes"), Some(61 << 30));
    }

    #[test]
    fn work_db_is_a_sample_store_that_prunes_by_cutoff() {
        let db = WorkDb::open_in_memory().unwrap();
        let store: &dyn SampleStore = &db;
        for ts in [10, 20, 30] {
            store
                .insert_host_load_sample(&HostSample {
                    ts_ms: ts,
                    ..HostSample::default()
                })
                .unwrap();
        }
        assert_eq!(store.prune_host_load_samples(25).unwrap(), 2);
        assert_eq!(db.host_load_latest().unwrap().map(|s| s.ts_ms), Some(30));
    }
}
