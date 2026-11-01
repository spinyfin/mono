//! Recovery-pointer snapshot/restore telemetry owned by storage.
use std::time::Instant;

use boss_metrics::{CounterHandle, GaugeHandle, Registry};

boss_metrics::register_gauge!(
    EXECUTION_BOOKMARK_POINTERS_PASS_DURATION_MS,
    "execution_bookmark_pointers.pass_duration_ms",
    "Wall-clock duration in milliseconds of the most recent recovery-pointer snapshot or restore over a PR chain.",
);
boss_metrics::register_gauge!(
    EXECUTION_BOOKMARK_POINTERS_ITEMS_PER_PASS,
    "execution_bookmark_pointers.items_per_pass",
    "Recovery-bookmark records the most recent pointer snapshot or restore covered.",
);
boss_metrics::register_counter!(
    EXECUTION_BOOKMARK_POINTERS_PASSES,
    "execution_bookmark_pointers.passes",
    "Total recovery-pointer snapshot and restore passes since engine start.",
);
boss_metrics::register_counter!(
    EXECUTION_BOOKMARK_POINTERS_JJ_SPAWNS,
    "execution_bookmark_pointers.jj_spawns",
    "Total `jj` child processes spawned by recovery-pointer snapshot and restore passes since engine start.",
);

pub struct SweepTicker {
    pass_duration_ms: &'static GaugeHandle,
    items_per_pass: &'static GaugeHandle,
    passes: &'static CounterHandle,
}

pub struct PassTimer {
    ticker: &'static SweepTicker,
    started: Instant,
}

impl SweepTicker {
    const fn new(
        pass_duration_ms: &'static GaugeHandle,
        items_per_pass: &'static GaugeHandle,
        passes: &'static CounterHandle,
    ) -> Self {
        Self {
            pass_duration_ms,
            items_per_pass,
            passes,
        }
    }

    pub fn begin(&'static self) -> PassTimer {
        PassTimer {
            ticker: self,
            started: Instant::now(),
        }
    }
}

impl PassTimer {
    pub fn finish(self, registry: &Registry, items: usize) {
        self.ticker.pass_duration_ms.set(
            registry,
            i64::try_from(self.started.elapsed().as_millis()).unwrap_or(i64::MAX),
        );
        self.ticker
            .items_per_pass
            .set(registry, i64::try_from(items).unwrap_or(i64::MAX));
        self.ticker.passes.inc(registry);
    }
}

pub static EXECUTION_BOOKMARK_POINTERS: SweepTicker = SweepTicker::new(
    &EXECUTION_BOOKMARK_POINTERS_PASS_DURATION_MS,
    &EXECUTION_BOOKMARK_POINTERS_ITEMS_PER_PASS,
    &EXECUTION_BOOKMARK_POINTERS_PASSES,
);

pub fn register_metrics(registry: &Registry) {
    registry.register_gauge(&EXECUTION_BOOKMARK_POINTERS_PASS_DURATION_MS);
    registry.register_gauge(&EXECUTION_BOOKMARK_POINTERS_ITEMS_PER_PASS);
    registry.register_counter(&EXECUTION_BOOKMARK_POINTERS_PASSES);
    registry.register_counter(&EXECUTION_BOOKMARK_POINTERS_JJ_SPAWNS);
}
