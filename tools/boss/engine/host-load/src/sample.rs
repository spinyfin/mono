//! The value one sampler tick produces and the retained series stores.

use serde::{Deserialize, Serialize};

/// One host-load observation.
///
/// Every measurement is `Option` because each comes from an independent
/// kernel call that can fail, and because the CPU percentages need a
/// previous tick to take a delta against — the first sample after startup
/// has none. A `None` means "not measured", never "zero".
///
/// `ts_ms` is epoch milliseconds, UTC. It is the only timestamp stored; the
/// CLI converts to local time for display.
#[derive(bon::Builder, Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostSample {
    pub ts_ms: i64,
    pub logical_cpus: Option<i64>,
    /// `vm.loadavg` 1 / 5 / 15 minute averages.
    pub load_avg_1m: Option<f64>,
    pub load_avg_5m: Option<f64>,
    pub load_avg_15m: Option<f64>,
    /// Percentages of CPU time over the interval since the previous
    /// sample. `user` includes nice time; `busy` is `100 - idle`.
    pub cpu_busy_pct: Option<f64>,
    pub cpu_user_pct: Option<f64>,
    pub cpu_system_pct: Option<f64>,
    pub cpu_idle_pct: Option<f64>,
    pub mem_total_bytes: Option<i64>,
    /// Activity-Monitor-style "memory used": app + wired + compressed.
    pub mem_used_bytes: Option<i64>,
    /// Physical memory the compressor occupies.
    pub mem_compressor_bytes: Option<i64>,
    /// Pages on the free list (excluding speculative).
    pub mem_free_bytes: Option<i64>,
    /// `total - used`: what could be handed out without paging.
    pub mem_available_bytes: Option<i64>,
    /// `kern.memorystatus_level`: the kernel's own "percent of memory
    /// available" figure, 0-100. Low means pressure.
    pub mem_pressure_level_pct: Option<i64>,
    pub swap_total_bytes: Option<i64>,
    pub swap_used_bytes: Option<i64>,
}
