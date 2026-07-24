use super::*;
use boss_protocol::WorkerActivity;

fn live(slot: u8, run: &str) -> LiveWorkerState {
    LiveWorkerState {
        slot_id: slot,
        name: boss_protocol::name_for_slot(slot),
        run_id: run.into(),
        model: "opus".into(),
        shell_pid: 0,
        last_event_at: None,
        current_tool: None,
        last_tool_ended_at: None,
        activity: WorkerActivity::Idle,
        work_item_id: None,
        work_item_name: None,
        execution_id: None,
        live_status: None,
        live_status_at: None,
        recovery_status: None,
        held: false,
    }
}

// NOTE: unit tests for the `agents.rs` reference-resolution and
// candidate-formatting helpers (`resolve_agent_ref`, `pick_unique`,
// `live_candidates_summary`, `looks_like_name_or_slot`,
// `WorkItem::primary_id`) now live co-located in that module's own
// `#[cfg(test)] mod tests` — see `agents.rs`. This crate-level module
// keeps the tests for the `logs.rs` dispatch-tail helpers and the
// bossctl-boundary `LiveWorkerState` serialization guard.

fn ev(ts: u128, stage: &str, outcome: &str, exec: &str) -> DispatchEvent {
    DispatchEvent {
        ts_epoch_ms: ts,
        stage: stage.into(),
        outcome: outcome.into(),
        execution_id: exec.into(),
        work_item_id: None,
        worker_id: None,
        cube_repo_id: None,
        cube_lease_id: None,
        cube_workspace_id: None,
        cube_command: None,
        cube_cwd: None,
        error_message: None,
        details: serde_json::Value::Null,
    }
}

#[test]
fn filter_and_tail_returns_last_n() {
    let events = vec![
        ev(1, "request_recorded", "ok", "e1"),
        ev(2, "worker_claimed", "ok", "e1"),
        ev(3, "cube_repo_ensured", "ok", "e1"),
        ev(4, "cube_workspace_leased", "ok", "e1"),
        ev(5, "pane_spawned", "ok", "e1"),
    ];
    let slice = filter_and_tail(&events, 2, None, None);
    assert_eq!(slice.len(), 2);
    assert_eq!(slice[0].stage, "cube_workspace_leased");
    assert_eq!(slice[1].stage, "pane_spawned");
}

#[test]
fn filter_and_tail_filters_stage_and_outcome() {
    let events = vec![
        ev(1, "request_recorded", "ok", "e1"),
        ev(2, "pane_spawned", "ok", "e1"),
        ev(3, "pane_spawned", "error", "e2"),
        ev(4, "pane_spawned", "error", "e3"),
    ];
    let slice = filter_and_tail(&events, 10, Some("pane_spawned"), Some("error"));
    assert_eq!(slice.len(), 2);
    assert_eq!(slice[0].execution_id, "e2");
    assert_eq!(slice[1].execution_id, "e3");
}

#[test]
fn build_tail_json_round_trips_events_as_array() {
    let events = vec![
        ev(1, "request_recorded", "ok", "e1"),
        ev(2, "pane_spawned", "error", "e1"),
    ];
    let slice = filter_and_tail(&events, 10, None, None);
    let json = build_tail_json(slice);
    let arr = json.get("events").and_then(|v| v.as_array()).unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["stage"], "request_recorded");
    assert_eq!(arr[1]["outcome"], "error");
}

#[test]
fn build_diagnose_json_attaches_stage_duration_ms_to_each_event() {
    let events = vec![
        ev(100, "request_recorded", "ok", "e1"),
        ev(450, "pane_spawned", "ok", "e1"),
    ];
    let durations = vec![350u128, 0u128];
    let json = build_diagnose_json("e1", &events, &durations);
    assert_eq!(json["execution_id"], "e1");
    let arr = json["events"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["stage_duration_ms"], 350);
    assert_eq!(arr[1]["stage_duration_ms"], 0);
    assert_eq!(arr[0]["stage"], "request_recorded");
}

#[test]
fn build_diagnose_json_returns_empty_events_when_none() {
    let json = build_diagnose_json("exec-missing", &[], &[]);
    assert_eq!(json["execution_id"], "exec-missing");
    assert!(json["events"].as_array().unwrap().is_empty());
}

/// Re-assert PR #340's invariant at the *bossctl* boundary — the
/// path the user's `agents list --json` actually flows through.
/// The protocol crate has its own test; this one catches a future
/// refactor that swaps the wire shape (or wraps it in a struct
/// that re-derives the serialization without `#[serde(default)]`).
/// The chore description specifically called out that the running
/// engine's output on the user's machine did not include these
/// keys.
#[test]
fn live_state_json_always_includes_live_status_keys_at_bossctl_boundary() {
    // `agents list --json` uses `serde_json::json!({...})` to
    // wrap a Vec<LiveWorkerState> — exercise the same wrapper.
    let states = vec![live(7, "exec_dead")];
    let payload = serde_json::json!({ "live_worker_states": states });
    let text = serde_json::to_string(&payload).unwrap();
    assert!(
        text.contains("\"live_status\":null"),
        "missing live_status key in bossctl serialization: {text}"
    );
    assert!(
        text.contains("\"live_status_at\":null"),
        "missing live_status_at key in bossctl serialization: {text}"
    );

    // `agents status <name>` uses `print_live_state` which
    // serializes a single state directly. Pin that path too.
    let single = serde_json::to_string(&states[0]).unwrap();
    assert!(
        single.contains("\"live_status\":null"),
        "missing live_status key in single-state serialization: {single}"
    );
    assert!(
        single.contains("\"live_status_at\":null"),
        "missing live_status_at key in single-state serialization: {single}"
    );
}

#[test]
fn format_age_ms_never_for_non_positive_timestamp() {
    // A zero or negative timestamp means "never seen".
    assert_eq!(format_age_ms(0, 10_000), "(never)");
    assert_eq!(format_age_ms(-5, 10_000), "(never)");
}

#[test]
fn format_age_ms_just_now_when_now_precedes_timestamp() {
    // Clock skew: `now` is earlier than the event timestamp.
    assert_eq!(format_age_ms(5_000, 1_000), "(just now)");
}

#[test]
fn format_age_ms_seconds_bucket_below_a_minute() {
    // ts = 25s, now = 30s => 5s of age.
    assert_eq!(format_age_ms(25_000, 30_000), "(5s ago)");
    // 59s is still reported in seconds.
    assert_eq!(format_age_ms(1_000, 60_000), "(59s ago)");
}

#[test]
fn format_age_ms_crosses_into_minutes_at_60s() {
    // Exactly 60s of age rolls over to "(1m ago)".
    assert_eq!(format_age_ms(1_000, 61_000), "(1m ago)");
    // 59m is still reported in minutes.
    assert_eq!(format_age_ms(1_000, 3_541_000), "(59m ago)");
}

#[test]
fn format_age_ms_crosses_into_hours_at_60m() {
    // Exactly 60m (3_600_000 ms) of age rolls over to "(1h ago)".
    assert_eq!(format_age_ms(1_000, 3_601_000), "(1h ago)");
    // 23h is still reported in hours.
    assert_eq!(format_age_ms(1_000, 82_801_000), "(23h ago)");
}

#[test]
fn format_age_ms_crosses_into_days_at_24h() {
    // Exactly 24h (86_400_000 ms) of age rolls over to "(1d ago)".
    assert_eq!(format_age_ms(1_000, 86_401_000), "(1d ago)");
}

#[test]
fn format_age_ms_reports_multiple_days() {
    // 3 days of age.
    assert_eq!(format_age_ms(1_000, 259_201_000), "(3d ago)");
}

#[test]
fn pause_system_all_covers_every_registry_variant() {
    // `PauseSystem::all()` drives the default scope of `bossctl
    // pause`/`bossctl resume` with no arguments — pin that it always
    // matches clap's enumeration of variants (the registry), not a
    // hand-maintained list that could drift when a variant is added.
    let all = PauseSystem::all();
    assert_eq!(all, <PauseSystem as clap::ValueEnum>::value_variants());
}

#[test]
fn pause_arg_targets_defaults_to_every_system_when_empty() {
    assert_eq!(pause_arg_targets(&[]), PauseSystem::all());
}

#[test]
fn pause_arg_targets_filters_out_the_state_sentinel() {
    // `state` is handled before this function is ever called (it
    // dispatches to `unified_state` instead), but the filter should
    // still drop it defensively rather than mapping to a phantom system.
    let targets = pause_arg_targets(&[PauseArg::Dispatch, PauseArg::State]);
    assert_eq!(targets, vec![PauseSystem::Dispatch]);
}

#[test]
fn pause_arg_targets_preserves_explicit_subset_and_order() {
    let targets = pause_arg_targets(&[PauseArg::Automation]);
    assert_eq!(targets, vec![PauseSystem::Automation]);
}

#[test]
fn format_dispatch_set_line_matches_existing_dispatch_pause_text() {
    let paused = DispatchPauseState {
        paused: true,
        paused_since_epoch_s: Some(123),
        reviews_exempt: true,
    };
    assert_eq!(
        format_dispatch_set_line(&paused),
        "dispatch paused (since epoch 123) — PR reviews are exempt and keep dispatching"
    );

    let resumed = DispatchPauseState {
        paused: false,
        paused_since_epoch_s: None,
        reviews_exempt: false,
    };
    assert_eq!(format_dispatch_set_line(&resumed), "dispatch resumed");
}

#[test]
fn format_dispatch_set_line_flags_non_exempt_breaker_pause() {
    let paused = DispatchPauseState {
        paused: true,
        paused_since_epoch_s: None,
        reviews_exempt: false,
    };
    assert_eq!(
        format_dispatch_set_line(&paused),
        "dispatch paused — PR reviews are held too (spawn-capability breaker)"
    );
}

#[test]
fn format_automation_set_line_matches_existing_automation_pause_text() {
    let paused = AutomationPauseState {
        paused: true,
        paused_since_epoch_s: Some(456),
    };
    assert_eq!(
        format_automation_set_line(&paused),
        "automation paused (since epoch 456) — new triage passes and automation-pool spawns are held; \
         already-running automation workers finish normally"
    );

    let resumed = AutomationPauseState {
        paused: false,
        paused_since_epoch_s: None,
    };
    assert_eq!(format_automation_set_line(&resumed), "automation resumed");
}

#[test]
fn format_state_summary_reports_paused_with_and_without_since() {
    assert_eq!(format_state_summary(true, Some(789)), "paused (since epoch 789)");
    assert_eq!(format_state_summary(true, None), "paused");
    assert_eq!(format_state_summary(false, None), "running");
}
