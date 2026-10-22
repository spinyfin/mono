//! Per-pass telemetry for the engine's background sweeps.
//!
//! Duration, item counts and pass counts describe the work performed.
//! Periodic singleton sweeps also report the interval between pass starts.
//! Concurrent tails and rebase operations do not have a shared cadence.

use std::sync::Mutex;
use std::time::Instant;

use crate::metrics::{CounterHandle, GaugeHandle, Registry};

crate::register_gauge!(
    ENVELOPE_WATCH_PASS_DURATION_MS,
    "envelope_watch.pass_duration_ms",
    "Wall-clock duration in milliseconds of the most recent envelope-watch sweep pass.",
);
crate::register_gauge!(
    ENVELOPE_WATCH_ITEMS_PER_PASS,
    "envelope_watch.items_per_pass",
    "Live worker slots the most recent envelope-watch pass evaluated.",
);
crate::register_gauge!(
    ENVELOPE_WATCH_TICK_INTERVAL_MS,
    "envelope_watch.tick_interval_ms",
    "Milliseconds between the starts of the two most recent envelope-watch passes.",
);
crate::register_counter!(
    ENVELOPE_WATCH_PASSES,
    "envelope_watch.passes",
    "Total envelope-watch sweep passes since engine start.",
);

crate::register_gauge!(
    TERMINAL_WORK_SWEEP_PASS_DURATION_MS,
    "terminal_work_sweep.pass_duration_ms",
    "Wall-clock duration in milliseconds of the most recent terminal-work sweep's candidate-gathering phase.",
);
crate::register_gauge!(
    TERMINAL_WORK_SWEEP_ITEMS_PER_PASS,
    "terminal_work_sweep.items_per_pass",
    "Live worker slots the most recent terminal-work sweep pass inspected.",
);
crate::register_gauge!(
    TERMINAL_WORK_SWEEP_TICK_INTERVAL_MS,
    "terminal_work_sweep.tick_interval_ms",
    "Milliseconds between the starts of the two most recent terminal-work sweep passes.",
);
crate::register_counter!(
    TERMINAL_WORK_SWEEP_PASSES,
    "terminal_work_sweep.passes",
    "Total terminal-work sweep passes since engine start.",
);

crate::register_gauge!(
    AGENT_JSONL_STREAM_PASS_DURATION_MS,
    "agent_jsonl_stream.pass_duration_ms",
    "Wall-clock duration in milliseconds of the most recent agent-JSONL tail poll iteration (any live stream).",
);
crate::register_gauge!(
    AGENT_JSONL_STREAM_ITEMS_PER_PASS,
    "agent_jsonl_stream.items_per_pass",
    "JSONL-root revalidations (lstat + canonicalize) the most recent tail poll iteration performed.",
);
crate::register_counter!(
    AGENT_JSONL_STREAM_PASSES,
    "agent_jsonl_stream.passes",
    "Total agent-JSONL tail poll iterations across every live stream since engine start.",
);
crate::register_counter!(
    AGENT_JSONL_STREAM_ROOT_REVALIDATIONS,
    "agent_jsonl_stream.root_revalidations",
    "Total JSONL-root revalidations (lstat + canonicalize) performed by tail polls since engine start.",
);

crate::register_gauge!(
    EXECUTION_BOOKMARK_POINTERS_PASS_DURATION_MS,
    "execution_bookmark_pointers.pass_duration_ms",
    "Wall-clock duration in milliseconds of the most recent recovery-pointer snapshot or restore over a PR chain.",
);
crate::register_gauge!(
    EXECUTION_BOOKMARK_POINTERS_ITEMS_PER_PASS,
    "execution_bookmark_pointers.items_per_pass",
    "Recovery-bookmark records the most recent pointer snapshot or restore covered.",
);
crate::register_counter!(
    EXECUTION_BOOKMARK_POINTERS_PASSES,
    "execution_bookmark_pointers.passes",
    "Total recovery-pointer snapshot and restore passes since engine start.",
);
crate::register_counter!(
    EXECUTION_BOOKMARK_POINTERS_JJ_SPAWNS,
    "execution_bookmark_pointers.jj_spawns",
    "Total `jj` child processes spawned by recovery-pointer snapshot and restore passes since engine start.",
);

crate::register_gauge!(
    DEAD_PANE_TERMINAL_PASS_DURATION_MS,
    "dead_pane_terminal.pass_duration_ms",
    "Wall-clock milliseconds of the most recent terminal execution scan and process probes.",
);
crate::register_gauge!(
    DEAD_PANE_TERMINAL_ITEMS_PER_PASS,
    "dead_pane_terminal.items_per_pass",
    "Recent terminal execution rows probed by the most recent scan.",
);
crate::register_counter!(
    DEAD_PANE_TERMINAL_PASSES,
    "dead_pane_terminal.passes",
    "Total terminal execution scans since engine start.",
);
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
        &EXECUTION_BOOKMARK_POINTERS,
    ] {
        registry.register_gauge(ticker.pass_duration_ms);
        registry.register_gauge(ticker.items_per_pass);
        if let Some(interval) = ticker.tick_interval_ms {
            registry.register_gauge(interval);
        }
        registry.register_counter(ticker.passes);
    }
    registry.register_counter(&AGENT_JSONL_STREAM_ROOT_REVALIDATIONS);
    registry.register_counter(&EXECUTION_BOOKMARK_POINTERS_JJ_SPAWNS);
}

/// The four handles one sweep publishes, plus the start instant of its
/// previous pass (for the tick-interval gauge). One static per sweep; a
/// pass is bracketed by [`SweepTicker::begin`] / [`PassTimer::finish`].
pub(crate) struct SweepTicker {
    pass_duration_ms: &'static GaugeHandle,
    items_per_pass: &'static GaugeHandle,
    tick_interval_ms: Option<&'static GaugeHandle>,
    passes: &'static CounterHandle,
    last_pass_started: Mutex<Option<Instant>>,
}

impl SweepTicker {
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
            last_pass_started: Mutex::new(None),
        }
    }

    /// Mark the start of a pass. Records the gap since the previous pass
    /// started (the observed tick interval) and returns the timer whose
    /// [`PassTimer::finish`] records duration and item count.
    pub(crate) fn begin(&'static self, registry: &Registry) -> PassTimer {
        let now = Instant::now();
        let previous = if self.tick_interval_ms.is_some() {
            let mut last = self
                .last_pass_started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            last.replace(now)
        } else { None };
        if let (Some(previous), Some(interval)) = (previous, self.tick_interval_ms) {
            interval
                .set(registry, saturating_ms(now.duration_since(previous)));
        }
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
pub(crate) static EXECUTION_BOOKMARK_POINTERS: SweepTicker = SweepTicker::new(
    &EXECUTION_BOOKMARK_POINTERS_PASS_DURATION_MS,
    &EXECUTION_BOOKMARK_POINTERS_ITEMS_PER_PASS,
    None,
    &EXECUTION_BOOKMARK_POINTERS_PASSES,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticker_records_duration_items_interval_and_pass_count() {
        let registry = Registry::new();
        register_metrics(&registry);

        let first = ENVELOPE_WATCH.begin(&registry);
        first.finish(&registry, 3);
        assert_eq!(registry.gauge_value("envelope_watch.items_per_pass"), Some(3));
        assert_eq!(registry.counter_value("envelope_watch.passes"), Some(1));
        assert!(registry.gauge_value("envelope_watch.pass_duration_ms").is_some());

        let second = ENVELOPE_WATCH.begin(&registry);
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
}
