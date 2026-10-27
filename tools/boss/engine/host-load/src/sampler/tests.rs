use super::*;
use crate::probe::SwapReading;

fn ticks(user: u32, system: u32, idle: u32, nice: u32) -> CpuTicks {
    CpuTicks {
        user,
        system,
        idle,
        nice,
    }
}

fn approx(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} != {b}");
}

#[test]
fn first_sample_has_no_previous_tick_so_no_percentages() {
    assert_eq!(cpu_percent_delta(None, ticks(10, 10, 80, 0)), None);
}

#[test]
fn percentages_come_from_the_delta_not_the_cumulative_counters() {
    let prev = ticks(1_000, 500, 98_500, 0);
    // +30 user, +10 system, +60 idle over the interval.
    let now = ticks(1_030, 510, 98_560, 0);
    let p = cpu_percent_delta(Some(prev), now).unwrap();
    approx(p.user, 30.0);
    approx(p.system, 10.0);
    approx(p.idle, 60.0);
    approx(p.busy, 40.0);
}

#[test]
fn nice_time_counts_as_user() {
    let p = cpu_percent_delta(Some(ticks(0, 0, 0, 0)), ticks(10, 0, 80, 10)).unwrap();
    approx(p.user, 20.0);
    approx(p.idle, 80.0);
    approx(p.busy, 20.0);
}

#[test]
fn counters_that_wrapped_still_yield_the_true_delta() {
    let prev = ticks(u32::MAX - 4, 0, u32::MAX - 49, 0);
    // user wrapped: +10 (5 to the top, 5 more past zero); idle wrapped: +90.
    let now = ticks(5, 0, 40, 0);
    let p = cpu_percent_delta(Some(prev), now).unwrap();
    approx(p.user, 10.0);
    approx(p.idle, 90.0);
    approx(p.busy, 10.0);
}

#[test]
fn no_elapsed_ticks_is_none_rather_than_a_divide_by_zero() {
    let t = ticks(7, 7, 7, 7);
    assert_eq!(cpu_percent_delta(Some(t), t), None);
}

fn vm() -> VmReading {
    VmReading {
        page_size: 16_384,
        free_pages: 1_000,
        speculative_pages: 200,
        internal_pages: 10_000,
        purgeable_pages: 1_000,
        wired_pages: 2_000,
        compressor_pages: 3_000,
    }
}

#[test]
fn memory_is_app_plus_wired_plus_compressed_and_sums_to_total() {
    let total = 100_000 * 16_384;
    let m = derive_memory(&vm(), total);
    // app = 10_000 - 1_000, + wired 2_000, + compressor 3_000 = 14_000 pages.
    assert_eq!(m.used_bytes, 14_000 * 16_384);
    assert_eq!(m.compressor_bytes, 3_000 * 16_384);
    assert_eq!(m.free_bytes, 800 * 16_384);
    assert_eq!(m.used_bytes + m.available_bytes, total);
}

#[test]
fn used_memory_never_exceeds_the_physical_total() {
    let total = 5_000 * 16_384;
    let m = derive_memory(&vm(), total);
    assert_eq!(m.used_bytes, total);
    assert_eq!(m.available_bytes, 0);
}

/// Scripted CPU ticks with fixed results for the other probe readings.
#[derive(bon::Builder)]
struct FakeProbe {
    cpus: Result<u32, ProbeError>,
    load: Result<[f64; 3], ProbeError>,
    ticks: Vec<Result<CpuTicks, ProbeError>>,
    vm: Result<VmReading, ProbeError>,
    total: Result<u64, ProbeError>,
    level: Result<i64, ProbeError>,
    swap: Result<SwapReading, ProbeError>,
    cpu_calls: usize,
}

impl FakeProbe {
    fn healthy(ticks: Vec<CpuTicks>) -> Self {
        Self::builder()
            .cpus(Ok(12))
            .load(Ok([155.0, 170.5, 185.0]))
            .ticks(ticks.into_iter().map(Ok).collect())
            .vm(Ok(vm()))
            .total(Ok(100_000 * 16_384))
            .level(Ok(7))
            .swap(Ok(SwapReading {
                total_bytes: 12 << 30,
                used_bytes: 10 << 30,
            }))
            .cpu_calls(0)
            .build()
    }
}

impl HostProbe for FakeProbe {
    fn logical_cpus(&mut self) -> Result<u32, ProbeError> {
        self.cpus
    }
    fn load_avg(&mut self) -> Result<[f64; 3], ProbeError> {
        self.load
    }
    fn cpu_ticks(&mut self) -> Result<CpuTicks, ProbeError> {
        let i = self.cpu_calls.min(self.ticks.len() - 1);
        self.cpu_calls += 1;
        self.ticks[i]
    }
    fn vm(&mut self) -> Result<VmReading, ProbeError> {
        self.vm
    }
    fn mem_total_bytes(&mut self) -> Result<u64, ProbeError> {
        self.total
    }
    fn mem_pressure_level_pct(&mut self) -> Result<i64, ProbeError> {
        self.level
    }
    fn swap(&mut self) -> Result<SwapReading, ProbeError> {
        self.swap
    }
}

#[test]
fn first_tick_fills_everything_but_cpu_and_second_tick_adds_cpu() {
    let mut s = HostSampler::new(FakeProbe::healthy(vec![ticks(0, 0, 0, 0), ticks(25, 25, 50, 0)]));

    let first = s.sample(1_000);
    assert_eq!(first.ts_ms, 1_000);
    assert_eq!(first.logical_cpus, Some(12));
    assert_eq!(first.load_avg_5m, Some(170.5));
    assert_eq!(first.cpu_busy_pct, None);
    assert_eq!(first.cpu_idle_pct, None);
    assert_eq!(first.mem_total_bytes, Some(100_000 * 16_384));
    assert_eq!(first.mem_pressure_level_pct, Some(7));
    assert_eq!(first.swap_used_bytes, Some(10 << 30));

    let second = s.sample(61_000);
    approx(second.cpu_busy_pct.unwrap(), 50.0);
    approx(second.cpu_user_pct.unwrap(), 25.0);
    approx(second.cpu_system_pct.unwrap(), 25.0);
    approx(second.cpu_idle_pct.unwrap(), 50.0);
    assert!(s.reported_failures().is_empty());
}

#[test]
fn a_failing_call_blanks_only_its_fields_and_is_reported_once() {
    let mut probe = FakeProbe::healthy(vec![ticks(0, 0, 0, 0)]);
    probe.swap = Err(ProbeError::errno("vm.swapusage", 12));
    probe.vm = Err(ProbeError::kern("host_statistics64(HOST_VM_INFO64)", 4));
    let mut s = HostSampler::new(probe);

    for ts in [1_000, 61_000, 121_000] {
        let sample = s.sample(ts);
        assert_eq!(sample.swap_total_bytes, None);
        assert_eq!(sample.swap_used_bytes, None);
        assert_eq!(sample.mem_used_bytes, None);
        // Independent calls are unaffected.
        assert_eq!(sample.load_avg_1m, Some(155.0));
        assert_eq!(sample.mem_total_bytes, Some(100_000 * 16_384));
    }
    // Three failing ticks, each of two calls: one report per call, not per tick.
    let reported: Vec<_> = s.reported_failures().iter().copied().collect();
    assert_eq!(reported, vec!["host_statistics64(HOST_VM_INFO64)", "vm.swapusage"]);
}

#[test]
fn a_cpu_tick_failure_does_not_discard_the_previous_reading() {
    let mut probe = FakeProbe::healthy(vec![ticks(0, 0, 0, 0)]);
    probe.ticks = vec![
        Ok(ticks(0, 0, 0, 0)),
        Err(ProbeError::kern("host_statistics(HOST_CPU_LOAD_INFO)", 5)),
        Ok(ticks(10, 10, 80, 0)),
    ];
    let mut s = HostSampler::new(probe);
    assert_eq!(s.sample(1).cpu_busy_pct, None);
    assert_eq!(s.sample(2).cpu_busy_pct, None);
    // The delta spans the failed tick, back to the last good reading.
    approx(s.sample(3).cpu_busy_pct.unwrap(), 20.0);
}

#[test]
fn logical_cpu_count_is_retried_until_it_resolves() {
    let mut probe = FakeProbe::healthy(vec![ticks(0, 0, 0, 0)]);
    probe.cpus = Err(ProbeError::errno("hw.logicalcpu", 1));
    let mut s = HostSampler::new(probe);
    assert_eq!(s.sample(1).logical_cpus, None);
    s.probe.cpus = Ok(8);
    assert_eq!(s.sample(2).logical_cpus, Some(8));
}
