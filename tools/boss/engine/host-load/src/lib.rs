//! Host-load sampling for the engine: how busy is this machine, measured
//! rather than inferred.
//!
//! Records host load so contention can be correlated with execution timing. Once per
//! interval (default 60 s) it takes one [`HostSample`] — load averages,
//! CPU busy/user/system/idle percentages, memory and compressor sizes,
//! swap, a memory-pressure indicator — hands it to an observer (the
//! engine publishes gauges) and appends it to a retained series through a
//! [`SampleStore`] (the engine backs this with a `state.db` table).
//!
//! ## Syscalls only
//!
//! Sampling never spawns a process (`vm_stat`, `memory_pressure`, `top`,
//! `ps`, `sysctl`, `uptime` are all off the table): forking on a saturated,
//! swapping host costs tens of milliseconds and adds the very load being
//! measured. [`DarwinProbe`] makes a handful of direct kernel calls per
//! tick — `sysctl` through MIBs resolved once, and `host_statistics*`.
//! Everything behind the [`HostProbe`] trait is a plain value read, so the
//! delta math and failure handling in [`HostSampler`] are tested with
//! fakes and need no real host values.
//!
//! ## Failure
//!
//! A failing syscall leaves its fields `None` in that sample, is logged at
//! WARN once per call (with the errno or Mach return code), and the
//! sampler keeps running. Nothing here can affect dispatch.

use serde::{Deserialize, Serialize};

mod darwin;
mod local_time;
mod probe;
mod run;
mod sampler;

pub use darwin::platform_probe;
pub use local_time::format_local_ms;
pub use probe::{CpuTicks, ErrorSource, HostProbe, ProbeError, SwapReading, VmReading};
pub use run::{
    DEFAULT_INTERVAL_SECONDS, DEFAULT_RETENTION_DAYS, LoopConfig, MIN_INTERVAL_SECONDS, MIN_RETENTION_DAYS,
    SampleObserver, SampleStore, spawn_sampler,
};
pub use sampler::{CpuPercents, HostSampler, cpu_percent_delta, derive_memory};

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
