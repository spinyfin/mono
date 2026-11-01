//! Per-pass telemetry for the engine's background sweeps.
//!
//! Duration, item counts and pass counts describe the work performed.
//! Periodic singleton sweeps also report the interval between pass starts.
//! Concurrent tails and rebase operations do not have a shared cadence.

use std::time::Instant;

use crate::metrics::{CounterHandle, GaugeHandle, Registry};
pub(crate) use crate::work::sweep_handles::AGENT_JSONL_STREAM_ROOT_REVALIDATIONS;
use crate::work::sweep_handles::{
    AGENT_JSONL_STREAM_ITEMS_PER_PASS, AGENT_JSONL_STREAM_PASS_DURATION_MS, AGENT_JSONL_STREAM_PASSES,
    DEAD_PANE_TERMINAL_ITEMS_PER_PASS, DEAD_PANE_TERMINAL_PASS_DURATION_MS, DEAD_PANE_TERMINAL_PASSES,
    ENVELOPE_WATCH_ITEMS_PER_PASS, ENVELOPE_WATCH_PASS_DURATION_MS, ENVELOPE_WATCH_PASSES,
    ENVELOPE_WATCH_TICK_INTERVAL_MS, TERMINAL_WORK_SWEEP_ITEMS_PER_PASS, TERMINAL_WORK_SWEEP_PASS_DURATION_MS,
    TERMINAL_WORK_SWEEP_PASSES, TERMINAL_WORK_SWEEP_TICK_INTERVAL_MS,
};
pub(crate) static DEAD_PANE_TERMINAL: SweepTicker = SweepTicker::new(
    &DEAD_PANE_TERMINAL_PASS_DURATION_MS,
    &DEAD_PANE_TERMINAL_ITEMS_PER_PASS,
    None,
    &DEAD_PANE_TERMINAL_PASSES,
);

/// Register every handle this module declares. Called from
/// [`crate::metrics_init::init_all`] at engine startup.
pub fn register_metrics(registry: &Registry) {
    for ticker in [
        &DEAD_PANE_TERMINAL,
        &ENVELOPE_WATCH,
        &TERMINAL_WORK_SWEEP,
        &AGENT_JSONL_STREAM,
    ] {
        registry.register_gauge(ticker.pass_duration_ms);
        registry.register_gauge(ticker.items_per_pass);
        if let Some(interval) = ticker.tick_interval_ms {
            registry.register_gauge(interval);
        }
        registry.register_counter(ticker.passes);
    }
    registry.register_counter(&AGENT_JSONL_STREAM_ROOT_REVALIDATIONS);
    crate::work::execution_bookmark_metrics::register_metrics(registry);
}

/// Handles published by one sweep. Cadence state is owned by its loop; a
/// pass is bracketed by [`SweepTicker::begin`] / [`PassTimer::finish`].
pub(crate) struct SweepTicker {
    pass_duration_ms: &'static GaugeHandle,
    items_per_pass: &'static GaugeHandle,
    tick_interval_ms: Option<&'static GaugeHandle>,
    passes: &'static CounterHandle,
}

impl SweepTicker {
    /// Cadence belongs to one running loop, separate from phase timing.
    pub(crate) fn record_tick(&self, registry: &Registry, previous: &mut Option<Instant>) {
        let now = Instant::now();
        if let (Some(last), Some(interval)) = (previous.replace(now), self.tick_interval_ms) {
            interval.set(registry, saturating_ms(now.duration_since(last)));
        }
    }

    const fn new(
        pass_duration_ms: &'static GaugeHandle,
        items_per_pass: &'static GaugeHandle,
        tick_interval_ms: Option<&'static GaugeHandle>,
        passes: &'static CounterHandle,
    ) -> Self {
        Self {
            pass_duration_ms,
            items_per_pass,
            tick_interval_ms,
            passes,
        }
    }

    /// Start timing the measured phase of a pass.
    pub(crate) fn begin(&'static self) -> PassTimer {
        let now = Instant::now();
        PassTimer {
            ticker: self,
            started: now,
        }
    }
}

/// An in-flight pass of one sweep; see [`SweepTicker::begin`].
pub(crate) struct PassTimer {
    ticker: &'static SweepTicker,
    started: Instant,
}

impl PassTimer {
    /// Record the pass's duration and how many items it touched.
    pub(crate) fn finish(self, registry: &Registry, items: usize) {
        self.ticker
            .pass_duration_ms
            .set(registry, saturating_ms(self.started.elapsed()));
        self.ticker
            .items_per_pass
            .set(registry, i64::try_from(items).unwrap_or(i64::MAX));
        self.ticker.passes.inc(registry);
    }
}

fn saturating_ms(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

pub(crate) static ENVELOPE_WATCH: SweepTicker = SweepTicker::new(
    &ENVELOPE_WATCH_PASS_DURATION_MS,
    &ENVELOPE_WATCH_ITEMS_PER_PASS,
    Some(&ENVELOPE_WATCH_TICK_INTERVAL_MS),
    &ENVELOPE_WATCH_PASSES,
);
pub(crate) static TERMINAL_WORK_SWEEP: SweepTicker = SweepTicker::new(
    &TERMINAL_WORK_SWEEP_PASS_DURATION_MS,
    &TERMINAL_WORK_SWEEP_ITEMS_PER_PASS,
    Some(&TERMINAL_WORK_SWEEP_TICK_INTERVAL_MS),
    &TERMINAL_WORK_SWEEP_PASSES,
);
pub(crate) static AGENT_JSONL_STREAM: SweepTicker = SweepTicker::new(
    &AGENT_JSONL_STREAM_PASS_DURATION_MS,
    &AGENT_JSONL_STREAM_ITEMS_PER_PASS,
    None,
    &AGENT_JSONL_STREAM_PASSES,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticker_records_duration_items_interval_and_pass_count() {
        let registry = Registry::new();
        register_metrics(&registry);

        let mut previous = None;
        ENVELOPE_WATCH.record_tick(&registry, &mut previous);
        let first = ENVELOPE_WATCH.begin();
        first.finish(&registry, 3);
        assert_eq!(registry.gauge_value("envelope_watch.items_per_pass"), Some(3));
        assert_eq!(registry.counter_value("envelope_watch.passes"), Some(1));
        assert!(registry.gauge_value("envelope_watch.pass_duration_ms").is_some());

        ENVELOPE_WATCH.record_tick(&registry, &mut previous);
        let second = ENVELOPE_WATCH.begin();
        second.finish(&registry, 0);
        assert_eq!(registry.gauge_value("envelope_watch.items_per_pass"), Some(0));
        assert_eq!(registry.counter_value("envelope_watch.passes"), Some(2));
        // The interval gauge is populated only once a previous pass exists.
        assert!(registry.gauge_value("envelope_watch.tick_interval_ms").is_some());
    }

    #[test]
    fn every_sweep_ticker_registers_distinct_names() {
        let registry = Registry::new();
        register_metrics(&registry);
        for name in [
            "envelope_watch.pass_duration_ms",
            "terminal_work_sweep.items_per_pass",
            "agent_jsonl_stream.items_per_pass",
            "execution_bookmark_pointers.pass_duration_ms",
        ] {
            assert_eq!(registry.gauge_value(name), Some(0), "{name}");
        }
        for name in [
            "envelope_watch.passes",
            "terminal_work_sweep.passes",
            "agent_jsonl_stream.passes",
            "agent_jsonl_stream.root_revalidations",
            "execution_bookmark_pointers.passes",
            "execution_bookmark_pointers.jj_spawns",
        ] {
            assert_eq!(registry.counter_value(name), Some(0), "{name}");
        }
    }

    #[test]
    fn direct_passes_do_not_change_loop_cadence() {
        let registry = Registry::new();
        register_metrics(&registry);
        let mut previous = Some(Instant::now() - std::time::Duration::from_secs(1));
        ENVELOPE_WATCH.record_tick(&registry, &mut previous);
        let interval = registry.gauge_value("envelope_watch.tick_interval_ms");
        ENVELOPE_WATCH.begin().finish(&registry, 0);
        ENVELOPE_WATCH.begin().finish(&registry, 0);
        assert_eq!(registry.gauge_value("envelope_watch.tick_interval_ms"), interval);
        let mut other_loop = None;
        ENVELOPE_WATCH.record_tick(&registry, &mut other_loop);
        assert_eq!(registry.gauge_value("envelope_watch.tick_interval_ms"), interval);
    }
}
