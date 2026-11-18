//! Sweep telemetry handles registered on a standalone `WorkDb` registry.
//!
//! Background sweeps publish per-pass gauges through `WorkDb::metrics_registry`.
//! Production injects the engine registry (registered by `metrics_init::init_all`);
//! unit tests open a bare `WorkDb` and need the same names present. The handles
//! live here so storage can register them without depending on engine core.

use boss_metrics::Registry;

boss_metrics::register_gauge!(
    ENVELOPE_WATCH_PASS_DURATION_MS,
    "envelope_watch.pass_duration_ms",
    "Wall-clock duration in milliseconds of the most recent envelope-watch sweep pass.",
);
boss_metrics::register_gauge!(
    ENVELOPE_WATCH_ITEMS_PER_PASS,
    "envelope_watch.items_per_pass",
    "Live worker slots the most recent envelope-watch pass evaluated.",
);
boss_metrics::register_gauge!(
    ENVELOPE_WATCH_TICK_INTERVAL_MS,
    "envelope_watch.tick_interval_ms",
    "Milliseconds between the starts of the two most recent envelope-watch passes.",
);
boss_metrics::register_counter!(
    ENVELOPE_WATCH_PASSES,
    "envelope_watch.passes",
    "Total envelope-watch sweep passes since engine start.",
);

boss_metrics::register_gauge!(
    TERMINAL_WORK_SWEEP_PASS_DURATION_MS,
    "terminal_work_sweep.pass_duration_ms",
    "Wall-clock duration in milliseconds of the most recent terminal-work sweep's candidate-gathering phase.",
);
boss_metrics::register_gauge!(
    TERMINAL_WORK_SWEEP_ITEMS_PER_PASS,
    "terminal_work_sweep.items_per_pass",
    "Live worker slots the most recent terminal-work sweep pass inspected.",
);
boss_metrics::register_gauge!(
    TERMINAL_WORK_SWEEP_TICK_INTERVAL_MS,
    "terminal_work_sweep.tick_interval_ms",
    "Milliseconds between the starts of the two most recent terminal-work sweep passes.",
);
boss_metrics::register_counter!(
    TERMINAL_WORK_SWEEP_PASSES,
    "terminal_work_sweep.passes",
    "Total terminal-work sweep passes since engine start.",
);

boss_metrics::register_gauge!(
    AGENT_JSONL_STREAM_PASS_DURATION_MS,
    "agent_jsonl_stream.pass_duration_ms",
    "Wall-clock duration in milliseconds of the most recent agent-JSONL tail poll iteration (any live stream).",
);
boss_metrics::register_gauge!(
    AGENT_JSONL_STREAM_ITEMS_PER_PASS,
    "agent_jsonl_stream.items_per_pass",
    "JSONL-root revalidations (lstat + canonicalize) the most recent tail poll iteration performed.",
);
boss_metrics::register_counter!(
    AGENT_JSONL_STREAM_PASSES,
    "agent_jsonl_stream.passes",
    "Total agent-JSONL tail poll iterations across every live stream since engine start.",
);
boss_metrics::register_counter!(
    AGENT_JSONL_STREAM_ROOT_REVALIDATIONS,
    "agent_jsonl_stream.root_revalidations",
    "Total JSONL-root revalidations (lstat + canonicalize) performed by tail polls since engine start.",
);

boss_metrics::register_gauge!(
    DEAD_PANE_TERMINAL_PASS_DURATION_MS,
    "dead_pane_terminal.pass_duration_ms",
    "Wall-clock milliseconds of the most recent terminal execution scan and process probes.",
);
boss_metrics::register_gauge!(
    DEAD_PANE_TERMINAL_ITEMS_PER_PASS,
    "dead_pane_terminal.items_per_pass",
    "Recent terminal execution rows probed by the most recent scan.",
);
boss_metrics::register_counter!(
    DEAD_PANE_TERMINAL_PASSES,
    "dead_pane_terminal.passes",
    "Total terminal execution scans since engine start.",
);

/// Register the sweep handles a standalone `WorkDb` registry must expose.
pub fn register_metrics(registry: &Registry) {
    registry.register_gauge(&ENVELOPE_WATCH_PASS_DURATION_MS);
    registry.register_gauge(&ENVELOPE_WATCH_ITEMS_PER_PASS);
    registry.register_gauge(&ENVELOPE_WATCH_TICK_INTERVAL_MS);
    registry.register_counter(&ENVELOPE_WATCH_PASSES);
    registry.register_gauge(&TERMINAL_WORK_SWEEP_PASS_DURATION_MS);
    registry.register_gauge(&TERMINAL_WORK_SWEEP_ITEMS_PER_PASS);
    registry.register_gauge(&TERMINAL_WORK_SWEEP_TICK_INTERVAL_MS);
    registry.register_counter(&TERMINAL_WORK_SWEEP_PASSES);
    registry.register_gauge(&AGENT_JSONL_STREAM_PASS_DURATION_MS);
    registry.register_gauge(&AGENT_JSONL_STREAM_ITEMS_PER_PASS);
    registry.register_counter(&AGENT_JSONL_STREAM_PASSES);
    registry.register_counter(&AGENT_JSONL_STREAM_ROOT_REVALIDATIONS);
    registry.register_gauge(&DEAD_PANE_TERMINAL_PASS_DURATION_MS);
    registry.register_gauge(&DEAD_PANE_TERMINAL_ITEMS_PER_PASS);
    registry.register_counter(&DEAD_PANE_TERMINAL_PASSES);
}
