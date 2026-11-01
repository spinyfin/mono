//! Recovery-pointer snapshot/restore telemetry owned by storage.
use boss_metrics::Registry;
use boss_metrics::sweep::SweepTicker;

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

pub static EXECUTION_BOOKMARK_POINTERS: SweepTicker = SweepTicker::new(
    &EXECUTION_BOOKMARK_POINTERS_PASS_DURATION_MS,
    &EXECUTION_BOOKMARK_POINTERS_ITEMS_PER_PASS,
    None,
    &EXECUTION_BOOKMARK_POINTERS_PASSES,
);

pub fn register_metrics(registry: &Registry) {
    EXECUTION_BOOKMARK_POINTERS.register(registry);
    registry.register_counter(&EXECUTION_BOOKMARK_POINTERS_JJ_SPAWNS);
}
