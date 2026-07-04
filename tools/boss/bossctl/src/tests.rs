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
    }
}

#[test]
fn resolves_by_run_id() {
    let states = vec![live(1, "exec_a"), live(2, "exec_b")];
    let hit = resolve_agent_ref("exec_b", &states).unwrap();
    assert_eq!(hit.slot_id, 2);
}

#[test]
fn resolves_by_numeric_slot_id() {
    let states = vec![live(1, "exec_a"), live(3, "exec_c")];
    let hit = resolve_agent_ref("3", &states).unwrap();
    assert_eq!(hit.run_id, "exec_c");
}

#[test]
fn resolves_by_crew_name_case_insensitive() {
    let states = vec![live(1, "exec_a"), live(2, "exec_b")];
    // slot 1 = Riker, slot 2 = Data
    let hit = resolve_agent_ref("riker", &states).unwrap();
    assert_eq!(hit.slot_id, 1);
    let hit = resolve_agent_ref("DATA", &states).unwrap();
    assert_eq!(hit.slot_id, 2);
}

#[test]
fn resolves_la_forge_with_space() {
    // Slot 4 is "La Forge" — the space matters for exact match.
    let states = vec![live(4, "exec_d")];
    let hit = resolve_agent_ref("la forge", &states).unwrap();
    assert_eq!(hit.slot_id, 4);
}

#[test]
fn unknown_reference_lists_live_candidates() {
    let states = vec![live(1, "exec_a"), live(2, "exec_b")];
    let err = resolve_agent_ref("Wesley", &states).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("no live worker matches"), "msg: {msg}");
    assert!(msg.contains("Riker"), "msg: {msg}");
    assert!(msg.contains("Data"), "msg: {msg}");
}

#[test]
fn unknown_with_no_live_workers_says_so() {
    let states: Vec<LiveWorkerState> = vec![];
    let err = resolve_agent_ref("Riker", &states).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("no live workers"), "msg: {msg}");
}

#[test]
fn run_id_takes_precedence_over_slot_match() {
    // If a run_id happens to be the literal "1", the run_id tier
    // wins before slot_id is even consulted (a defensive case
    // since real run ids are not numeric strings).
    let mut s1 = live(2, "1");
    // Force a different slot so a slot match would resolve to a
    // different worker; run_id "1" should still win.
    s1.slot_id = 2;
    let states = vec![s1, live(1, "exec_a")];
    let hit = resolve_agent_ref("1", &states).unwrap();
    assert_eq!(hit.run_id, "1");
    assert_eq!(hit.slot_id, 2);
}

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

#[test]
fn looks_like_name_or_slot_recognises_roster_and_numbers() {
    assert!(looks_like_name_or_slot("Riker"));
    assert!(looks_like_name_or_slot("riker"));
    assert!(looks_like_name_or_slot("La Forge"));
    assert!(looks_like_name_or_slot("3"));
    assert!(!looks_like_name_or_slot("exec_18ad6336fedcb190_12"));
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
fn live_candidates_summary_empty_says_no_live_workers() {
    let states: Vec<LiveWorkerState> = vec![];
    assert_eq!(live_candidates_summary(&states), "no live workers");
}

#[test]
fn live_candidates_summary_sorts_entries_by_slot_id() {
    // Provide slots out of order so the sort is observable in the
    // output: slot 3 appears before slot 1 in the input, but the
    // summary must list slot 1 first.
    let states = vec![live(3, "exec_c"), live(1, "exec_a")];
    let s1 = boss_protocol::name_for_slot(1);
    let s3 = boss_protocol::name_for_slot(3);
    assert_eq!(
        live_candidates_summary(&states),
        format!("Live: slot 1 ({s1}), slot 3 ({s3})"),
    );
}

#[test]
fn pick_unique_returns_the_sole_match() {
    let states = vec![live(1, "exec_a")];
    let matches = vec![&states[0]];
    let hit = pick_unique("riker", matches, &states).unwrap();
    assert_eq!(hit.slot_id, 1);
    assert_eq!(hit.run_id, "exec_a");
}

#[test]
fn pick_unique_errors_and_enumerates_all_ambiguous_matches() {
    let states = vec![live(1, "exec_a"), live(2, "exec_b")];
    let matches: Vec<&LiveWorkerState> = states.iter().collect();
    let err = pick_unique("opus", matches, &states).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("matches multiple live workers"), "msg: {msg}");
    // Every ambiguous match is enumerated with slot, name and run id.
    let s1 = boss_protocol::name_for_slot(1);
    let s2 = boss_protocol::name_for_slot(2);
    assert!(msg.contains(&format!("slot 1 ({s1}) run exec_a")), "msg: {msg}");
    assert!(msg.contains(&format!("slot 2 ({s2}) run exec_b")), "msg: {msg}");
    // The live-candidates summary is appended to the error.
    assert!(msg.contains("Live: "), "msg: {msg}");
}
