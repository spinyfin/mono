use super::*;
use boss_protocol::{CreateChoreInput, CreateProductInput, WorkItemPatch, WorkerActivity};

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
        pool: None,
        kind: None,
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
fn build_timeline_json_attaches_stage_duration_ms_to_each_event() {
    let events = vec![
        ev(100, "request_recorded", "ok", "e1"),
        ev(450, "pane_spawned", "ok", "e1"),
    ];
    let durations = vec![350u128, 0u128];
    let json = doctor::build_timeline_json("e1", &events, &durations);
    assert_eq!(json["execution_id"], "e1");
    let arr = json["events"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["stage_duration_ms"], 350);
    assert_eq!(arr[1]["stage_duration_ms"], 0);
    assert_eq!(arr[0]["stage"], "request_recorded");
}

#[test]
fn build_timeline_json_returns_empty_events_when_none() {
    let json = doctor::build_timeline_json("exec-missing", &[], &[]);
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
    assert_eq!(pause::pause_arg_targets(&[]), PauseSystem::all());
}

#[test]
fn pause_arg_targets_filters_out_the_state_sentinel() {
    // `state` is handled before this function is ever called (it
    // dispatches to `unified_state` instead), but the filter should
    // still drop it defensively rather than mapping to a phantom system.
    let targets = pause::pause_arg_targets(&[PauseArg::Dispatch, PauseArg::State]);
    assert_eq!(targets, vec![PauseSystem::Dispatch]);
}

#[test]
fn pause_arg_targets_preserves_explicit_subset_and_order() {
    let targets = pause::pause_arg_targets(&[PauseArg::Automation]);
    assert_eq!(targets, vec![PauseSystem::Automation]);
}

#[test]
fn format_dispatch_set_line_matches_existing_dispatch_pause_text() {
    let paused = pause::DispatchPauseState {
        paused: true,
        paused_since_epoch_s: Some(123),
        reviews_exempt: true,
        reason: Some("investigating a spike in failed dispatch attempts".to_string()),
    };
    assert_eq!(
        pause::format_dispatch_set_line(&paused),
        "dispatch paused (since epoch 123) — PR reviews are exempt and keep dispatching — \
         reason: investigating a spike in failed dispatch attempts"
    );

    let resumed = pause::DispatchPauseState {
        paused: false,
        paused_since_epoch_s: None,
        reviews_exempt: false,
        reason: None,
    };
    assert_eq!(pause::format_dispatch_set_line(&resumed), "dispatch resumed");
}

#[test]
fn format_dispatch_set_line_flags_non_exempt_breaker_pause() {
    let paused = pause::DispatchPauseState {
        paused: true,
        paused_since_epoch_s: None,
        reviews_exempt: false,
        reason: Some("spawn-capability circuit breaker tripped".to_string()),
    };
    assert_eq!(
        pause::format_dispatch_set_line(&paused),
        "dispatch paused — PR reviews are held too (spawn-capability breaker) — \
         reason: spawn-capability circuit breaker tripped"
    );
}

/// A breaker pause holds PR reviews, and the scope block must say so — plus
/// name every route that can still reach a spawn while it holds. The
/// 2026-08-10 incident was a breaker pause printing `reviews: held` while
/// its own recovery canary spent PR-review rows; the canary is now
/// review-ineligible, and the bypass that remains is declared here rather
/// than left for an operator to discover from a dispatch tail.
#[test]
fn dispatch_scope_lines_declare_a_breaker_pause_scope_and_its_bypasses() {
    let block = pause::dispatch_scope_lines(false).join("\n");
    assert!(block.contains("origin: breaker"), "{block}");
    assert!(block.contains("reviews: held"), "{block}");
    assert!(
        block.contains("never a PR review"),
        "the canary bypass must state that it cannot spend a review: {block}"
    );
    assert!(
        block.contains("bossctl agents launch"),
        "the explicit operator override must be declared: {block}"
    );
}

/// An operator pause genuinely exempts reviews, so the same block must say
/// *that* — the honesty requirement runs in both directions.
#[test]
fn dispatch_scope_lines_declare_an_operator_pause_review_exemption() {
    let block = pause::dispatch_scope_lines(true).join("\n");
    assert!(block.contains("origin: operator"), "{block}");
    assert!(block.contains("reviews: exempt"), "{block}");
    assert!(
        !block.contains("recovery canary"),
        "an operator pause is never auto-probed, so it must not advertise a canary: {block}"
    );
    assert!(
        block.contains("bossctl agents launch"),
        "the explicit operator override applies to every pause mode: {block}"
    );
}

#[test]
fn format_automation_set_line_matches_existing_automation_pause_text() {
    let paused = pause::AutomationPauseState {
        paused: true,
        paused_since_epoch_s: Some(456),
        reason: Some("investigating a spike in failed dispatch attempts".to_string()),
    };
    assert_eq!(
        pause::format_automation_set_line(&paused),
        "automation paused (since epoch 456) — reason: investigating a spike in failed dispatch \
         attempts — new triage passes and automation-pool spawns are held; already-running \
         automation workers finish normally"
    );

    let resumed = pause::AutomationPauseState {
        paused: false,
        paused_since_epoch_s: None,
        reason: None,
    };
    assert_eq!(pause::format_automation_set_line(&resumed), "automation resumed");
}

#[test]
fn format_state_summary_reports_paused_with_and_without_since() {
    assert_eq!(pause::format_state_summary(true, Some(789)), "paused (since epoch 789)");
    assert_eq!(pause::format_state_summary(true, None), "paused");
    assert_eq!(pause::format_state_summary(false, None), "running");
}

// ── metrics github: rate arithmetic ──────────────────────────────────────
//
// The rate is the whole reason this subcommand exists — GitHub's budget is
// 5000 units per rolling hour, and a monotonic counter cannot express that.
// These pin the denominator, which is the part that is easy to get wrong in
// a way that silently understates a drain.

#[test]
fn points_per_hour_divides_by_the_observed_span() {
    // 100 units over 30 observed minutes is 200/hr — regardless of how
    // many hours of look-back were requested.
    assert_eq!(points_per_hour(100, 30 * 60 * 1000), Some(200.0));
}

#[test]
fn points_per_hour_matches_the_merge_poller_estimate_shape() {
    // The figure this instrumentation exists to confirm or refute:
    // 6780 units observed over exactly one hour reads as 6780/hr, which
    // is over the 5000/hr budget.
    let rate = points_per_hour(6780, 3_600_000).expect("one hour is a usable span");
    assert!((rate - 6780.0).abs() < 0.001);
    assert!(rate > GITHUB_HOURLY_BUDGET, "6780/hr must read as over budget");
}

#[test]
fn points_per_hour_refuses_a_span_shorter_than_a_minute() {
    // Extrapolating one burst across an hour would produce a confident
    // wrong number; reporting nothing is the honest answer.
    assert_eq!(points_per_hour(500, 10_000), None);
    assert_eq!(points_per_hour(500, 0), None);
}

#[test]
fn points_per_hour_handles_zero_spend_over_a_real_span() {
    assert_eq!(points_per_hour(0, 3_600_000), Some(0.0));
}

// ── pause reason requirement ─────────────────────────────────────────────
//
// `--reason` must never be fabricated on a human's behalf: an operator, an
// agent, and a script pausing all record the same string under the old
// default, which destroys the one distinction the field exists to capture.
// `dispatch pause`/`automation pause` enforce this declaratively via clap
// (their `--reason` is a required `String`, not `Option<String>`); the
// unified `bossctl pause` enforces it in `require_pause_reason` since its
// clap field must stay optional for `bossctl pause state`.

#[test]
fn require_pause_reason_rejects_a_missing_reason() {
    assert!(pause::require_pause_reason(None).is_err());
}

#[test]
fn require_pause_reason_passes_through_an_explicit_reason() {
    assert_eq!(
        pause::require_pause_reason(Some("disk full".to_owned())).unwrap(),
        "disk full"
    );
}

#[test]
fn dispatch_pause_fails_to_parse_without_reason() {
    let result = Cli::try_parse_from(["bossctl", "dispatch", "pause"]);
    assert!(
        result.is_err(),
        "omitting --reason must fail clap parsing, not silently pause with a fabricated reason"
    );
}

#[test]
fn dispatch_pause_parses_an_explicit_reason() {
    let cli = Cli::try_parse_from(["bossctl", "dispatch", "pause", "--reason", "disk full"]).unwrap();
    match cli.command {
        Command::Dispatch {
            action: DispatchAction::Pause { reason },
        } => assert_eq!(reason, "disk full"),
        other => panic!("expected DispatchAction::Pause, got {other:?}"),
    }
}

#[test]
fn automation_pause_fails_to_parse_without_reason() {
    let result = Cli::try_parse_from(["bossctl", "automation", "pause"]);
    assert!(
        result.is_err(),
        "omitting --reason must fail clap parsing, not silently pause with a fabricated reason"
    );
}

#[test]
fn automation_pause_parses_an_explicit_reason() {
    let cli = Cli::try_parse_from(["bossctl", "automation", "pause", "--reason", "disk full"]).unwrap();
    match cli.command {
        Command::Automation {
            action: AutomationAction::Pause { reason },
        } => assert_eq!(reason, "disk full"),
        other => panic!("expected AutomationAction::Pause, got {other:?}"),
    }
}

#[test]
fn work_start_defaults_force_to_false() {
    let cli = Cli::try_parse_from(["bossctl", "work", "start", "task_1"]).unwrap();
    match cli.command {
        Command::Work {
            action: WorkAction::Start {
                work_item_id, force, ..
            },
        } => {
            assert_eq!(work_item_id, "task_1");
            assert!(!force, "--force must default to false");
        }
        other => panic!("expected WorkAction::Start, got {other:?}"),
    }
}

/// `agents launch`'s pool-growth `force` and `work start`'s pause-only
/// `force` must never collide on the wire: the CLI flag spelling is
/// shared, but each maps to a distinct `RequestExecutionInput` intent
/// (`force` vs `bypass_dispatch_pause`) — see `agents::agents_launch`
/// and `agents::work_start`.
#[test]
fn work_start_parses_force_flag() {
    let cli = Cli::try_parse_from(["bossctl", "work", "start", "task_1", "--force"]).unwrap();
    match cli.command {
        Command::Work {
            action: WorkAction::Start {
                work_item_id, force, ..
            },
        } => {
            assert_eq!(work_item_id, "task_1");
            assert!(force);
        }
        other => panic!("expected WorkAction::Start, got {other:?}"),
    }
}

#[test]
fn work_start_force_composes_with_priority_and_workspace() {
    let cli = Cli::try_parse_from([
        "bossctl",
        "work",
        "start",
        "task_1",
        "--force",
        "--priority",
        "5",
        "--preferred-workspace-id",
        "mono-agent-002",
    ])
    .unwrap();
    match cli.command {
        Command::Work {
            action:
                WorkAction::Start {
                    work_item_id,
                    priority,
                    preferred_workspace_id,
                    host,
                    force,
                },
        } => {
            assert_eq!(work_item_id, "task_1");
            assert!(force);
            assert_eq!(priority, Some(5));
            assert_eq!(preferred_workspace_id.as_deref(), Some("mono-agent-002"));
            assert!(host.is_none());
        }
        other => panic!("expected WorkAction::Start, got {other:?}"),
    }
}

#[test]
fn host_flag_is_available_on_both_manual_dispatch_verbs() {
    let work = Cli::try_parse_from(["bossctl", "work", "start", "task_1", "--host", "remote-a"]).unwrap();
    match work.command {
        Command::Work {
            action: WorkAction::Start { host, .. },
        } => assert_eq!(host.as_deref(), Some("remote-a")),
        other => panic!("expected WorkAction::Start, got {other:?}"),
    }

    let agents = Cli::try_parse_from(["bossctl", "agents", "launch", "task_1", "--host", "remote-a"]).unwrap();
    match agents.command {
        Command::Agents {
            action: AgentsAction::Launch { host, .. },
        } => assert_eq!(host.as_deref(), Some("remote-a")),
        other => panic!("expected AgentsAction::Launch, got {other:?}"),
    }
}

/// Walk clap collecting `(surface_path, arg_id, has_work_item_id_marker)`.
fn walk_clap_args(command: &clap::Command) -> Vec<(String, String, bool)> {
    fn walk(command: &clap::Command, path: &mut Vec<String>, out: &mut Vec<(String, String, bool)>) {
        for arg in command.get_arguments() {
            let arg_id = arg.get_id().as_str().to_owned();
            let marked = arg.get_value_names().is_some_and(|names| {
                names
                    .iter()
                    .any(|n| n.as_str() == boss_protocol::WORK_ITEM_ID_VALUE_NAME)
            });
            let mut surface = path.clone();
            surface.push(arg_id.clone());
            out.push((surface.join(" "), arg_id, marked));
        }
        for sub in command.get_subcommands() {
            path.push(sub.get_name().to_owned());
            walk(sub, path, out);
            path.pop();
        }
    }
    let mut out = Vec::new();
    walk(command, &mut Vec::new(), &mut out);
    out
}

fn is_id_shaped_arg(arg_id: &str) -> bool {
    if arg_id.starts_with("with_") || arg_id == "comment_id" {
        return false;
    }
    matches!(
        arg_id,
        "id" | "selector" | "parent" | "dependent" | "prerequisite" | "depends_on" | "task" | "project" | "agent"
    ) || arg_id.starts_with("work_item")
        || arg_id.ends_with("_id")
}

/// Arg ids that look id-shaped but are not work-item selectors.
const NON_WORK_ITEM_ARG_IDS: &[&str] = &[
    "execution_id",
    "probe_id",
    "preferred_workspace_id",
    "host_id",
    "agent", // run id / slot / crew name; work-item form is optional fall-through
];

/// Surface paths for non-work-item namespaces (hosts, comments cmt_…).
fn is_non_work_item_surface(surface: &str) -> bool {
    if surface.starts_with("hosts ") {
        return true;
    }
    if surface.starts_with("comments ") && (surface.ends_with(" comment_id") || surface.contains("comment_id")) {
        return true;
    }
    false
}

/// Adversarial: every id-shaped bossctl arg is marked WORK_ITEM_ID or
/// explicitly allowlisted as a non-work-item namespace.
#[test]
fn every_id_shaped_arg_is_marked_or_allowlisted() {
    use clap::CommandFactory;
    let args = walk_clap_args(&Cli::command());
    let mut unmarked = Vec::new();
    let mut marked = Vec::new();
    for (surface, arg_id, has_marker) in &args {
        if !is_id_shaped_arg(arg_id) {
            continue;
        }
        if *has_marker {
            marked.push(surface.clone());
            continue;
        }
        if NON_WORK_ITEM_ARG_IDS.contains(&arg_id.as_str()) || is_non_work_item_surface(surface) {
            continue;
        }
        // Positional `id` under dispatch diagnose is marked; other bare
        // `id` fields (if any) must be allowlisted here intentionally.
        unmarked.push(format!("{surface} (arg={arg_id})"));
    }
    assert!(
        unmarked.is_empty(),
        "id-shaped bossctl clap args missing WORK_ITEM_ID marker: {unmarked:?}\n\
         marked: {marked:?}"
    );
    assert!(
        marked.iter().any(|s| s.contains("diagnose")),
        "dispatch diagnose must be a WORK_ITEM_ID surface; marked={marked:?}"
    );
    assert!(
        marked
            .iter()
            .any(|s| s.contains("work_item_id") || s.contains("work_item")),
        "work proposals / executions / cancel work-item filters must be marked; marked={marked:?}"
    );
}

/// Each handler module that serves WORK_ITEM_ID surfaces must call the
/// shared WorkDb choke point (`resolve_work_item_ref` /
/// `resolve_work_item_ref_strict` / `resolve_diagnose_id`).
#[test]
fn every_work_item_id_surface_routes_through_shared_resolver() {
    use clap::CommandFactory;
    let marked: Vec<String> = walk_clap_args(&Cli::command())
        .into_iter()
        .filter(|(_, _, m)| *m)
        .map(|(s, _, _)| s)
        .collect();
    assert!(!marked.is_empty(), "expected WORK_ITEM_ID surfaces on bossctl");

    const HANDLER_MODULES: &[(&str, &str, &[&str])] = &[
        (
            "main.rs",
            include_str!("main.rs"),
            &[
                "resolve_work_item_ref_strict",
                "resolve_work_item_ref",
                "resolve_diagnose_id",
            ],
        ),
        (
            "agents.rs",
            include_str!("agents.rs"),
            &["resolve_work_item_ref", "GetWorkItem"],
        ),
        (
            "review.rs",
            include_str!("review.rs"),
            &["resolve_work_item_ref_strict", "resolve_work_item_ref"],
        ),
    ];
    for (name, src, symbols) in HANDLER_MODULES {
        let reachable = symbols.iter().any(|sym| src.contains(sym));
        assert!(
            reachable,
            "bossctl handler {name} must call a shared resolver symbol \
             {symbols:?}; marked surfaces={marked:?}"
        );
    }
}

#[test]
fn resolve_diagnose_id_rejects_unresolvable_short_id() {
    // Without a db, friendly short ids must hard-error — never pass
    // through as a literal that yields an empty diagnose result.
    let err = resolve_diagnose_id(None, &format!("T{}", 99)).unwrap_err().to_string();
    assert!(
        err.contains("could not resolve") || err.contains("state.db"),
        "expected hard resolve failure, got: {err}"
    );
}

#[test]
fn resolve_diagnose_id_passes_through_execution_ids() {
    let resolved = resolve_diagnose_id(None, "exec_18cb2cafec048218_1e").unwrap();
    assert_eq!(resolved, "exec_18cb2cafec048218_1e");
}

fn ghost_entry(exec: &str, work_item: Option<&str>) -> dispatch_reader::GhostActiveEntry {
    dispatch_reader::GhostActiveEntry::builder()
        .execution_id(exec)
        .maybe_work_item_id(work_item.map(str::to_owned))
        .last_stage("cube_change_created")
        .last_outcome("ok")
        .last_ts_epoch_ms(1_000u128)
        .elapsed_since_last_ms(9_000u128)
        .stalled(true)
        .build()
}

#[test]
fn drop_closed_work_items_keeps_everything_when_db_is_absent() {
    let entries = vec![
        ghost_entry("exec-open", Some("task_missing")),
        ghost_entry("exec-none", None),
    ];
    let kept = drop_closed_work_items(entries.clone(), None);
    assert_eq!(kept, entries);
}

#[test]
fn drop_closed_work_items_drops_terminal_work_items_and_keeps_open_or_unknown() {
    let db = WorkDb::open(":memory:".into()).unwrap();
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("ghost-filter")
                .repo_remote_url("git@github.com:test/ghost-filter.git")
                .build(),
        )
        .unwrap();
    let open = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("still open")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let closed = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("already done")
                .autostart(false)
                .build(),
        )
        .unwrap();
    db.update_work_item(
        &closed.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let entries = vec![
        ghost_entry("exec-open", Some(&open.id)),
        ghost_entry("exec-done", Some(&closed.id)),
        ghost_entry("exec-none", None),
        ghost_entry("exec-unknown", Some("task_does_not_exist")),
    ];
    let kept = drop_closed_work_items(entries, Some(&db));
    let ids: Vec<_> = kept.iter().map(|e| e.execution_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["exec-open", "exec-none", "exec-unknown"],
        "closed work items are dropped; missing ids and unbound executions stay: {kept:?}"
    );
}
