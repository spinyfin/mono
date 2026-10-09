//! Host-load sampling for the engine: how busy is this machine, measured
//! rather than inferred.
//!
//! A throughput analysis once had to *infer* host contention from slowdown
//! because nothing recorded load. This crate is the recorder. Once per
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

mod darwin;
mod local_time;
mod probe;
mod run;
mod sample;
mod sampler;

pub use darwin::platform_probe;
pub use local_time::format_local_ms;
pub use probe::{CpuTicks, ErrorSource, HostProbe, ProbeError, SwapReading, VmReading};
pub use run::{
    DEFAULT_INTERVAL_SECONDS, DEFAULT_RETENTION_DAYS, LoopConfig, MIN_INTERVAL_SECONDS, SampleObserver, SampleStore,
    spawn_sampler,
};
pub use sample::HostSample;
pub use sampler::{CpuPercents, HostSampler, cpu_percent_delta, derive_memory};
