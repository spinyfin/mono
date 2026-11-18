//! Shared pass timing and cadence for background sweeps.
use crate::{CounterHandle, GaugeHandle, Registry};
use std::time::Instant;

/// Handles published by one sweep. Cadence state is owned by its loop; a
/// pass is bracketed by [`SweepTicker::begin`] / [`PassTimer::finish`].
pub struct SweepTicker {
    pass_duration_ms: &'static GaugeHandle,
    items_per_pass: &'static GaugeHandle,
    tick_interval_ms: Option<&'static GaugeHandle>,
    passes: &'static CounterHandle,
}

impl SweepTicker {
    /// Register the handles published by this sweep.
    pub fn register(&self, registry: &Registry) {
        registry.register_gauge(self.pass_duration_ms);
        registry.register_gauge(self.items_per_pass);
        if let Some(interval) = self.tick_interval_ms {
            registry.register_gauge(interval);
        }
        registry.register_counter(self.passes);
    }

    /// Cadence belongs to one running loop, separate from phase timing.
    pub fn record_tick(&self, registry: &Registry, previous: &mut Option<Instant>) {
        let now = Instant::now();
        if let (Some(last), Some(interval)) = (previous.replace(now), self.tick_interval_ms) {
            interval.set(registry, saturating_ms(now.duration_since(last)));
        }
    }

    pub const fn new(
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
    pub fn begin(&'static self) -> PassTimer {
        let now = Instant::now();
        PassTimer {
            ticker: self,
            started: now,
        }
    }
}

/// An in-flight pass of one sweep; see [`SweepTicker::begin`].
pub struct PassTimer {
    ticker: &'static SweepTicker,
    started: Instant,
}

impl PassTimer {
    /// Record the pass's duration and how many items it touched.
    pub fn finish(self, registry: &Registry, items: usize) {
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
