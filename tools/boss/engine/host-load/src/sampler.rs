//! Turns raw probe readings into [`HostSample`]s.

use std::collections::BTreeSet;

use crate::probe::{CpuTicks, HostProbe, ProbeError, VmReading};
use crate::sample::HostSample;

/// CPU time split over one sampling interval, each in `0.0..=100.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuPercents {
    pub busy: f64,
    pub user: f64,
    pub system: f64,
    pub idle: f64,
}

/// CPU percentages from the tick deltas between two readings.
///
/// The kernel's counters are cumulative `u32`s that wrap, so each delta is
/// a `wrapping_sub` — correct as long as less than one full wrap elapses
/// between samples, which a per-minute interval comfortably satisfies.
/// Returns `None` for the first sample (no `prev`) and when no ticks
/// elapsed (a zero total would otherwise divide by zero). `nice` time is
/// folded into `user`, as Activity Monitor does.
pub fn cpu_percent_delta(prev: Option<CpuTicks>, now: CpuTicks) -> Option<CpuPercents> {
    let prev = prev?;
    let user = u64::from(now.user.wrapping_sub(prev.user)) + u64::from(now.nice.wrapping_sub(prev.nice));
    let system = u64::from(now.system.wrapping_sub(prev.system));
    let idle = u64::from(now.idle.wrapping_sub(prev.idle));
    let total = user + system + idle;
    if total == 0 {
        return None;
    }
    let pct = |part: u64| part as f64 * 100.0 / total as f64;
    Some(CpuPercents {
        busy: pct(user + system),
        user: pct(user),
        system: pct(system),
        idle: pct(idle),
    })
}

/// Memory figures derived from one VM reading plus the physical total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DerivedMemory {
    pub used_bytes: u64,
    pub compressor_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
}

/// Activity-Monitor-style accounting: *used* is app memory (anonymous pages
/// minus purgeable) + wired + compressor; *available* is `total - used`, so
/// the two always sum to the physical total; *free* is the raw free list
/// less speculative pages (what `vm_stat` calls "Pages free").
pub fn derive_memory(vm: &VmReading, total_bytes: u64) -> DerivedMemory {
    let bytes = |pages: u64| pages.saturating_mul(vm.page_size);
    let app_pages = vm.internal_pages.saturating_sub(vm.purgeable_pages);
    let used = bytes(app_pages)
        .saturating_add(bytes(vm.wired_pages))
        .saturating_add(bytes(vm.compressor_pages))
        .min(total_bytes);
    DerivedMemory {
        used_bytes: used,
        compressor_bytes: bytes(vm.compressor_pages),
        free_bytes: bytes(vm.free_pages.saturating_sub(vm.speculative_pages)),
        available_bytes: total_bytes - used,
    }
}

/// Stateful sampler: remembers the previous CPU tick reading for deltas and
/// which failing calls it has already complained about.
pub struct HostSampler<P: HostProbe> {
    probe: P,
    prev_ticks: Option<CpuTicks>,
    logical_cpus: Option<u32>,
    reported: BTreeSet<&'static str>,
}

impl<P: HostProbe> HostSampler<P> {
    pub fn new(probe: P) -> Self {
        Self {
            probe,
            prev_ticks: None,
            logical_cpus: None,
            reported: BTreeSet::new(),
        }
    }

    /// Names of the calls that have failed at least once (and so have been
    /// logged). Exposed for tests of the log-once behaviour.
    pub fn reported_failures(&self) -> &BTreeSet<&'static str> {
        &self.reported
    }

    /// Take one sample. Never fails: each probe error leaves its fields
    /// `None` and is logged once per call.
    pub fn sample(&mut self, ts_ms: i64) -> HostSample {
        let mut out = HostSample {
            ts_ms,
            ..HostSample::default()
        };

        // Resolved once; retried each tick only while it keeps failing.
        if self.logical_cpus.is_none() {
            self.logical_cpus = record(&mut self.reported, self.probe.logical_cpus());
        }
        out.logical_cpus = self.logical_cpus.map(i64::from);

        if let Some([a, b, c]) = record(&mut self.reported, self.probe.load_avg()) {
            out.load_avg_1m = Some(a);
            out.load_avg_5m = Some(b);
            out.load_avg_15m = Some(c);
        }

        if let Some(ticks) = record(&mut self.reported, self.probe.cpu_ticks()) {
            if let Some(p) = cpu_percent_delta(self.prev_ticks, ticks) {
                out.cpu_busy_pct = Some(p.busy);
                out.cpu_user_pct = Some(p.user);
                out.cpu_system_pct = Some(p.system);
                out.cpu_idle_pct = Some(p.idle);
            }
            self.prev_ticks = Some(ticks);
        }

        let total = record(&mut self.reported, self.probe.mem_total_bytes());
        out.mem_total_bytes = total.map(clamp_i64);
        if let (Some(vm), Some(total)) = (record(&mut self.reported, self.probe.vm()), total) {
            let m = derive_memory(&vm, total);
            out.mem_used_bytes = Some(clamp_i64(m.used_bytes));
            out.mem_compressor_bytes = Some(clamp_i64(m.compressor_bytes));
            out.mem_free_bytes = Some(clamp_i64(m.free_bytes));
            out.mem_available_bytes = Some(clamp_i64(m.available_bytes));
        }
        out.mem_pressure_level_pct = record(&mut self.reported, self.probe.mem_pressure_level_pct());

        if let Some(swap) = record(&mut self.reported, self.probe.swap()) {
            out.swap_total_bytes = Some(clamp_i64(swap.total_bytes));
            out.swap_used_bytes = Some(clamp_i64(swap.used_bytes));
        }
        out
    }
}

/// Unwrap a probe result, logging a call's first failure at WARN.
fn record<T>(reported: &mut BTreeSet<&'static str>, result: Result<T, ProbeError>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(err) => {
            if reported.insert(err.call) {
                tracing::warn!(
                    call = err.call,
                    code = err.code,
                    source = ?err.source,
                    "host_load: {err}; leaving those fields unset and continuing",
                );
            }
            None
        }
    }
}

fn clamp_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
