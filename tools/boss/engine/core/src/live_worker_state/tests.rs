use super::*;
use boss_protocol::StopReason;

fn pre_tool(tool: &str) -> WorkerEvent {
    WorkerEvent::PreToolUse {
        session_id: "s".into(),
        tool_name: tool.into(),
        tool_input: serde_json::Value::Null,
    }
}

fn post_tool(tool: &str) -> WorkerEvent {
    WorkerEvent::PostToolUse {
        session_id: "s".into(),
        tool_name: tool.into(),
        tool_input: serde_json::Value::Null,
        tool_response: serde_json::Value::Null,
    }
}

fn stop_event() -> WorkerEvent {
    WorkerEvent::Stop {
        session_id: "s".into(),
        stop_hook_active: false,
        stop_reason: StopReason::Completed,
    }
}

#[test]
fn update_shell_pid_finds_slot_by_run_id() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(3, "run-abc", "claude-opus-4-7", 0, None);
    let slot = reg.update_shell_pid("run-abc", 55555);
    assert_eq!(slot, Some(3));
    let state = reg.get(3).unwrap();
    assert_eq!(state.shell_pid, 55555);
}

#[test]
fn update_shell_pid_returns_none_for_unknown_run_id() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(3, "run-abc", "claude-opus-4-7", 0, None);
    let slot = reg.update_shell_pid("run-xyz", 99999);
    assert_eq!(slot, None);
    let state = reg.get(3).unwrap();
    assert_eq!(state.shell_pid, 0, "unmatched run must not be modified");
}

#[test]
fn register_spawn_creates_entry_with_spawning_activity() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(2, "run-1", "claude-opus-4-7", 12345, None);
    let state = reg.get(2).unwrap();
    assert_eq!(state.slot_id, 2);
    assert_eq!(state.run_id, "run-1");
    assert_eq!(state.model, "claude-opus-4-7");
    assert_eq!(state.shell_pid, 12345);
    assert_eq!(state.activity, WorkerActivity::Spawning);
    assert!(state.work_item_id.is_none());
    assert!(state.work_item_name.is_none());
    assert!(state.execution_id.is_none());
}

#[test]
fn activity_for_run_prefers_working_across_duplicate_live_slots() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.register_spawn(2, "run-1", "claude-opus-4-7", 2, None);
    reg.apply_event(
        1,
        &WorkerEvent::Stop {
            session_id: "s".into(),
            stop_hook_active: false,
            stop_reason: StopReason::Completed,
        },
    );
    reg.apply_event(
        2,
        &WorkerEvent::PreToolUse {
            session_id: "s".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
        },
    );

    assert_eq!(reg.activity_for_run("run-1"), Some(WorkerActivity::Working));
}

#[test]
fn register_spawn_with_binding_records_work_item_fields() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(
        2,
        "exec-1",
        "claude-opus-4-7",
        12345,
        Some(WorkItemBinding {
            work_item_id: "task_18ad1b81532ac910_4".into(),
            work_item_name: "Fix fencer scraping".into(),
            execution_id: "exec-1".into(),
        }),
    );
    let state = reg.get(2).unwrap();
    assert_eq!(state.work_item_id.as_deref(), Some("task_18ad1b81532ac910_4"));
    assert_eq!(state.work_item_name.as_deref(), Some("Fix fencer scraping"));
    assert_eq!(state.execution_id.as_deref(), Some("exec-1"));
    assert!(state.pool.is_none());
    assert!(state.kind.is_none());
}

#[test]
fn register_spawn_with_capabilities_stamps_pool_and_kind() {
    // Production spawn paths pass attributed pool + execution kind so
    // `bossctl agents list` can render them without joining the
    // execution table. Tests that use `register_spawn` leave both None.
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn_with_capabilities(
        2,
        "exec-1",
        "claude-opus-4-7",
        12345,
        Some(WorkItemBinding {
            work_item_id: "task_abc".into(),
            work_item_name: "Fix fencer scraping".into(),
            execution_id: "exec-1".into(),
        }),
        true,
        LiveSpawnRouting::new("automation", "chore_implementation"),
    );
    let state = reg.get(2).unwrap();
    assert_eq!(state.pool.as_deref(), Some("automation"));
    assert_eq!(state.kind.as_deref(), Some("chore_implementation"));
}

#[test]
fn attributed_pool_label_matches_coordinator_routing() {
    assert_eq!(attributed_pool_label(ExecutionKind::PrReview, false), "review");
    assert_eq!(attributed_pool_label(ExecutionKind::PrReview, true), "review");
    assert_eq!(attributed_pool_label(ExecutionKind::PrReviewGuide, false), "review");
    assert_eq!(attributed_pool_label(ExecutionKind::PrReviewGuide, true), "review");
    assert_eq!(
        attributed_pool_label(ExecutionKind::AutomationTriage, false),
        "automation"
    );
    assert_eq!(
        attributed_pool_label(ExecutionKind::TaskImplementation, true),
        "automation"
    );
    assert_eq!(attributed_pool_label(ExecutionKind::ChoreImplementation, false), "main");
    assert_eq!(
        attributed_pool_label(ExecutionKind::RevisionImplementation, false),
        "main"
    );
}

#[test]
fn release_slot_clears_entry() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    assert!(reg.get(1).is_some());
    reg.release_slot(1);
    assert!(reg.get(1).is_none());
}

#[test]
fn pre_tool_use_marks_working_with_tool_name() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    let changed = reg.apply_event(1, &pre_tool("Bash"));
    assert!(changed);
    let state = reg.get(1).unwrap();
    assert_eq!(state.activity, WorkerActivity::Working);
    assert_eq!(state.current_tool.as_deref(), Some("Bash"));
    assert!(state.last_event_at.is_some());
}

#[test]
fn post_tool_use_clears_current_tool_and_records_end_time() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.apply_event(1, &pre_tool("Bash"));
    reg.apply_event(1, &post_tool("Bash"));
    let state = reg.get(1).unwrap();
    assert!(state.current_tool.is_none());
    assert!(state.last_tool_ended_at.is_some());
    assert_eq!(state.activity, WorkerActivity::Working);
}

#[test]
fn stop_after_tools_transitions_to_idle() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.apply_event(1, &pre_tool("Bash"));
    reg.apply_event(1, &post_tool("Bash"));
    reg.apply_event(1, &stop_event());
    let state = reg.get(1).unwrap();
    assert_eq!(state.activity, WorkerActivity::Idle);
    assert!(state.current_tool.is_none());
}

#[test]
fn notification_then_stop_marks_waiting_for_input() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.apply_event(
        1,
        &WorkerEvent::Notification {
            session_id: "s".into(),
            message: "claude needs permission".into(),
        },
    );
    reg.apply_event(1, &stop_event());
    let state = reg.get(1).unwrap();
    assert_eq!(state.activity, WorkerActivity::WaitingForInput);
}

#[test]
fn pretooluse_after_notification_clears_pending_flag_and_marks_working() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.apply_event(
        1,
        &WorkerEvent::Notification {
            session_id: "s".into(),
            message: "permission".into(),
        },
    );
    reg.apply_event(1, &pre_tool("Edit"));
    reg.apply_event(1, &stop_event());
    let state = reg.get(1).unwrap();
    // Stop without a fresh notification should now be Idle.
    assert_eq!(state.activity, WorkerActivity::Idle);
}

#[test]
fn awaiting_input_incapable_driver_never_shows_waiting_for_input() {
    // A driver that doesn't declare `Capability::AwaitingInputSignal`
    // must never produce `WaitingForInput`, even if a `Notification`
    // event somehow arrives — the honest degrade is to leave activity
    // untouched (Working here) so `Stop` falls through to `Idle`,
    // never a fabricated `WaitingForInput`.
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.set_awaiting_input_capable(1, false);
    reg.apply_event(1, &pre_tool("Bash"));
    let before = reg.get(1).unwrap();
    assert_eq!(before.activity, WorkerActivity::Working);

    reg.apply_event(
        1,
        &WorkerEvent::Notification {
            session_id: "s".into(),
            message: "claude needs permission".into(),
        },
    );
    let after_notification = reg.get(1).unwrap();
    assert_eq!(
        after_notification.activity,
        WorkerActivity::Working,
        "an untrusted Notification must not change activity"
    );

    reg.apply_event(1, &stop_event());
    let after_stop = reg.get(1).unwrap();
    assert_eq!(
        after_stop.activity,
        WorkerActivity::Idle,
        "Stop must resolve to Idle, not a guessed WaitingForInput"
    );
}

#[test]
fn awaiting_input_capable_defaults_true_matching_claude_behaviour() {
    // Every existing caller of `register_spawn` (Claude is the only
    // production driver today) must see byte-identical behaviour
    // without calling `set_awaiting_input_capable` at all.
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.apply_event(
        1,
        &WorkerEvent::Notification {
            session_id: "s".into(),
            message: "claude needs permission".into(),
        },
    );
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
}

#[test]
fn release_slot_resets_awaiting_input_capable_to_default_on_respawn() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.set_awaiting_input_capable(1, false);
    reg.release_slot(1);
    reg.register_spawn(1, "run-2", "claude-opus-4-7", 1, None);
    reg.apply_event(
        1,
        &WorkerEvent::Notification {
            session_id: "s".into(),
            message: "claude needs permission".into(),
        },
    );
    assert_eq!(
        reg.get(1).unwrap().activity,
        WorkerActivity::WaitingForInput,
        "a fresh spawn into a recycled slot must not inherit the prior run's flag"
    );
}

#[test]
fn register_spawn_with_capabilities_seeds_flag_at_registration() {
    // The capability travels with registration in one call, so there is
    // no window where a hook event could race a follow-up setter call.
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn_with_capabilities(1, "run-1", "claude-opus-4-7", 1, None, false, LiveSpawnRouting::none());
    reg.apply_event(
        1,
        &WorkerEvent::Notification {
            session_id: "s".into(),
            message: "claude needs permission".into(),
        },
    );
    assert_eq!(
        reg.get(1).unwrap().activity,
        WorkerActivity::Spawning,
        "an untrusted Notification must not change activity"
    );
}

#[test]
fn set_awaiting_input_capable_is_a_noop_for_unregistered_slot() {
    let reg = LiveWorkerStateRegistry::new();
    // Must not panic when no entry exists for the slot (event/wiring
    // race ahead of spawn registration, or after release).
    reg.set_awaiting_input_capable(7, false);
    assert!(reg.get(7).is_none());
}

#[test]
fn session_end_marks_terminated() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.apply_event(
        1,
        &WorkerEvent::SessionEnd {
            session_id: "s".into(),
            reason: "exit".into(),
        },
    );
    let state = reg.get(1).unwrap();
    assert_eq!(state.activity, WorkerActivity::Terminated);
}

/// Regression test for the 2026-07-26 mass husk-retirement: a
/// `SessionEnd` that arrives while a `PreToolUse` is still unbalanced
/// must NOT erase the in-flight tool.
///
/// That unbalanced tool is the evidence
/// `husk_pane_sweep::live_process_evidence` uses to prove the worker is
/// still running before an irreversible kill. Clearing it here made a
/// worker inside a multi-minute foreground `bazel` build — which emits
/// no further hook by definition — indistinguishable from a genuinely
/// dead one, and five such workers were SIGTERMed mid-work.
#[test]
fn session_end_preserves_a_tool_still_in_flight() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 4242, None);
    reg.apply_event(1, &pre_tool("Bash"));
    reg.apply_event(
        1,
        &WorkerEvent::SessionEnd {
            session_id: "s".into(),
            reason: "other".into(),
        },
    );

    let state = reg.get(1).unwrap();
    assert_eq!(state.activity, WorkerActivity::Terminated);
    assert_eq!(
        state.current_tool.as_deref(),
        Some("Bash"),
        "an unbalanced PreToolUse must survive SessionEnd — it is the proof the process is still working",
    );
}

/// The normal path is unaffected: `Stop` already cleared the tool, so
/// a `SessionEnd` after a clean turn boundary still leaves it unset.
#[test]
fn session_end_after_stop_leaves_no_tool_in_flight() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 4242, None);
    reg.apply_event(1, &pre_tool("Bash"));
    reg.apply_event(1, &stop_event());
    reg.apply_event(
        1,
        &WorkerEvent::SessionEnd {
            session_id: "s".into(),
            reason: "other".into(),
        },
    );

    let state = reg.get(1).unwrap();
    assert_eq!(state.activity, WorkerActivity::Terminated);
    assert!(state.current_tool.is_none());
}

#[test]
fn session_start_startup_promotes_spawning_to_idle() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.apply_event(
        1,
        &WorkerEvent::SessionStart {
            session_id: "s".into(),
            source: SessionStartSource::Startup,
            model: None,
        },
    );
    let state = reg.get(1).unwrap();
    assert_eq!(state.activity, WorkerActivity::Idle);
}

#[test]
fn apply_event_returns_false_when_slot_not_registered() {
    let reg = LiveWorkerStateRegistry::new();
    let changed = reg.apply_event(7, &stop_event());
    assert!(!changed);
}

#[test]
fn apply_event_reports_unchanged_for_a_pure_heartbeat() {
    // A worker that is already `Idle` (no pending notification) and
    // receives a second `Stop` mutates nothing but `last_event_at`:
    // `current_tool` is already `None`, `activity` is already
    // `Idle`, `recovery_status` is already `None`. This is the
    // "hook event that advances only the timestamp" case the
    // broadcast-dedup gate exists to suppress.
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.apply_event(1, &pre_tool("Bash"));
    reg.apply_event(1, &post_tool("Bash"));
    let first_stop_changed = reg.apply_event(1, &stop_event());
    assert!(first_stop_changed, "Working -> Idle is a real change");
    reg.set_last_event_at_for_test(1, "2000-01-01T00:00:00Z");
    let before = reg.get(1).unwrap();

    let heartbeat_changed = reg.apply_event(1, &stop_event());

    assert!(
        !heartbeat_changed,
        "a repeated Stop while already Idle must not report a change"
    );
    let after = reg.get(1).unwrap();
    assert_ne!(before.last_event_at, after.last_event_at);
}

#[test]
fn apply_event_reports_unchanged_for_a_spurious_post_tool_use() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.apply_event(
        1,
        &WorkerEvent::UserPromptSubmit {
            session_id: "s".into(),
            prompt: "do the thing".into(),
        },
    );
    reg.apply_event(1, &post_tool("Bash"));
    reg.set_last_event_at_for_test(1, "2000-01-01T00:00:00Z");
    reg.set_last_tool_ended_at_for_test(1, "2000-01-01T00:00:00Z");
    let before = reg.get(1).unwrap();

    assert!(
        !reg.apply_event(1, &post_tool("Bash")),
        "a PostToolUse with no active tool must not report a timestamp-only change"
    );
    let after = reg.get(1).unwrap();
    assert_ne!(before.last_event_at, after.last_event_at);
    assert_ne!(before.last_tool_ended_at, after.last_tool_ended_at);
}

#[test]
fn apply_event_reports_changed_for_a_meaningful_field() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.apply_event(1, &stop_event());

    let changed = reg.apply_event(1, &pre_tool("Bash"));

    assert!(changed, "Idle -> Working (current_tool set) must report a change");
}

/// Replays a representative multi-worker trace through `apply_event`.
/// Each turn has two duplicate `Stop`s while idle plus a redundant
/// `Notification` while already waiting for input; those three events
/// must be suppressed. The first `Notification` is meaningful because
/// `register_spawn` enables awaiting-input capability by default.
#[test]
fn apply_event_dedup_broadcast_rate_over_a_multi_worker_trace() {
    const WORKER_COUNT: u8 = 6; // matches the six-worker incident noted on `SessionEnd` above.
    const TURNS_PER_WORKER: usize = 15;
    const TOOLS_PER_TURN: usize = 3;

    let reg = LiveWorkerStateRegistry::new();
    for slot in 1..=WORKER_COUNT {
        reg.register_spawn(slot, format!("run-{slot}"), "claude-opus-4-7", slot as i32, None);
    }

    let notification = || WorkerEvent::Notification {
        session_id: "s".into(),
        message: "permission prompt".into(),
    };

    let mut total_events = 0usize;
    let mut broadcast_events = 0usize;
    for slot in 1..=WORKER_COUNT {
        for _turn in 0..TURNS_PER_WORKER {
            let mut events: Vec<WorkerEvent> = vec![WorkerEvent::UserPromptSubmit {
                session_id: "s".into(),
                prompt: "do the thing".into(),
            }];
            for t in 0..TOOLS_PER_TURN {
                let tool = format!("Tool{t}");
                events.push(pre_tool(&tool));
                events.push(post_tool(&tool));
            }
            events.push(stop_event());
            // The first Notification promotes Idle to WaitingForInput;
            // the second is redundant. Force timestamp movement before
            // every no-op so this trace cannot pass due to the clock's
            // second-level granularity.
            events.push(stop_event());
            events.push(stop_event());
            events.push(notification());
            events.push(notification());

            for (index, event) in events.iter().enumerate() {
                if matches!(index, 8 | 9 | 11) {
                    reg.set_last_event_at_for_test(slot, "2000-01-01T00:00:00Z");
                }
                total_events += 1;
                if reg.apply_event(slot, event) {
                    broadcast_events += 1;
                }
            }
        }
    }

    let suppressed = total_events - broadcast_events;
    let broadcast_rate = broadcast_events as f64 / total_events as f64;
    println!(
        "apply_event dedup: {total_events} events, {broadcast_events} broadcasts, \
         {suppressed} suppressed ({:.1}% broadcast rate)",
        broadcast_rate * 100.0,
    );

    assert!(broadcast_events < total_events);
    assert_eq!(total_events, 1080);
    assert_eq!(broadcast_events, 810);
    assert_eq!(suppressed, 270);
}

#[test]
fn snapshot_returns_entries_sorted_by_slot() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(3, "run-3", "claude-opus-4-7", 0, None);
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 0, None);
    reg.register_spawn(2, "run-2", "claude-opus-4-7", 0, None);
    let states = reg.snapshot();
    assert_eq!(states.len(), 3);
    assert_eq!(states[0].slot_id, 1);
    assert_eq!(states[1].slot_id, 2);
    assert_eq!(states[2].slot_id, 3);
}

#[test]
fn set_live_status_writes_text_and_stamps_timestamp() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    let changed = reg.set_live_status(1, Some("running tests after the layout fix".into()));
    assert!(changed);
    let state = reg.get(1).unwrap();
    assert_eq!(state.live_status.as_deref(), Some("running tests after the layout fix"),);
    assert!(state.live_status_at.is_some());
}

#[test]
fn set_live_status_clears_both_fields() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.set_live_status(1, Some("doing a thing".into()));
    let changed = reg.set_live_status(1, None);
    assert!(changed);
    let state = reg.get(1).unwrap();
    assert!(state.live_status.is_none());
    assert!(state.live_status_at.is_none());
}

#[test]
fn set_live_status_returns_false_when_clearing_already_empty_slot() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    let changed = reg.set_live_status(1, None);
    assert!(!changed);
}

#[test]
fn set_live_status_returns_true_on_repeated_set_to_advance_timestamp() {
    // Two consecutive sets with the same text must still return
    // true so the broadcast fires — the staleness UI keys off
    // `live_status_at`, and freezing it on text equality would
    // misfire the "no summarizer activity" warning.
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    let first = reg.set_live_status(1, Some("running tests".into()));
    let second = reg.set_live_status(1, Some("running tests".into()));
    assert!(first);
    assert!(second);
}

#[test]
fn set_live_status_returns_false_when_slot_unknown() {
    let reg = LiveWorkerStateRegistry::new();
    let changed = reg.set_live_status(7, Some("orphan".into()));
    assert!(!changed);
}

#[test]
fn set_live_status_round_trips_through_snapshot() {
    // The snapshot is what the topic publisher serialises, so
    // confirm that a successful `set_live_status` shows up in
    // both the named getter and the snapshot list.
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(2, "run-2", "claude-opus-4-7", 0, None);
    reg.set_live_status(2, Some("editing the redactor".into()));
    let states = reg.snapshot();
    let s = states.iter().find(|s| s.slot_id == 2).unwrap();
    assert_eq!(s.live_status.as_deref(), Some("editing the redactor"));
    assert!(s.live_status_at.is_some());
}

#[test]
fn release_slot_clears_live_status_pair() {
    // Releasing a slot drops the entry whole, so a subsequent
    // re-spawn into the same slot starts with `None`/`None`.
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.set_live_status(1, Some("doing a thing".into()));
    reg.release_slot(1);
    assert!(reg.get(1).is_none());
    reg.register_spawn(1, "run-2", "claude-opus-4-7", 1, None);
    let state = reg.get(1).unwrap();
    assert!(state.live_status.is_none());
    assert!(state.live_status_at.is_none());
}

#[test]
fn set_recovery_status_writes_and_clears() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    let changed = reg.set_recovery_status(1, Some("recovering from API error (attempt 1/3)".into()));
    assert!(changed);
    assert_eq!(
        reg.get(1).unwrap().recovery_status.as_deref(),
        Some("recovering from API error (attempt 1/3)")
    );

    let changed = reg.set_recovery_status(1, None);
    assert!(changed);
    assert!(reg.get(1).unwrap().recovery_status.is_none());
}

#[test]
fn set_recovery_status_returns_false_when_slot_unknown_or_unchanged() {
    let reg = LiveWorkerStateRegistry::new();
    assert!(!reg.set_recovery_status(7, Some("recovering".into())));

    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    assert!(reg.set_recovery_status(1, Some("recovering".into())));
    // Setting the identical value again is a no-op.
    assert!(!reg.set_recovery_status(1, Some("recovering".into())));
}

#[test]
fn apply_event_clears_recovery_status_on_any_hook() {
    // Proof the worker's session is responsive again — any hook
    // event, not just one that flips activity to Working, must
    // clear a stale recovery banner so it never shadows real
    // progress or a normal idle-between-turns state.
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.set_recovery_status(1, Some("recovering from API error (attempt 1/3)".into()));
    assert!(reg.get(1).unwrap().recovery_status.is_some());

    reg.apply_event(1, &stop_event());
    assert!(
        reg.get(1).unwrap().recovery_status.is_none(),
        "recovery_status must clear on the next hook event"
    );
}

#[test]
fn release_slot_clears_recovery_status() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.set_recovery_status(1, Some("recovering from API error (attempt 1/3)".into()));
    reg.release_slot(1);
    assert!(reg.get(1).is_none());
    reg.register_spawn(1, "run-2", "claude-opus-4-7", 1, None);
    assert!(reg.get(1).unwrap().recovery_status.is_none());
}

#[test]
fn mark_errored_transitions_and_returns_changed() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    assert!(reg.mark_errored(1));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Errored);
    // Idempotent.
    assert!(!reg.mark_errored(1));
}

#[test]
fn run_id_for_work_item_finds_live_binding() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(
        3,
        "exec-42",
        "claude-opus-4-7",
        99,
        Some(WorkItemBinding {
            work_item_id: "chore_abc".into(),
            work_item_name: "My chore".into(),
            execution_id: "exec-42".into(),
        }),
    );
    assert_eq!(reg.run_id_for_work_item("chore_abc").as_deref(), Some("exec-42"));
    assert!(reg.run_id_for_work_item("chore_other").is_none());
}

#[test]
fn run_id_for_work_item_ignores_terminal_slots() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(
        1,
        "exec-dead",
        "claude-opus-4-7",
        10,
        Some(WorkItemBinding {
            work_item_id: "chore_xyz".into(),
            work_item_name: "Terminated chore".into(),
            execution_id: "exec-dead".into(),
        }),
    );
    reg.apply_event(
        1,
        &WorkerEvent::SessionEnd {
            session_id: "s".into(),
            reason: "exit".into(),
        },
    );
    assert!(reg.run_id_for_work_item("chore_xyz").is_none());
}

// ── mark_stalled_spawns (initial directory-trust prompt detection) ────────

/// Regression test for the initial-directory-trust-prompt detection path.
///
/// The directory-trust prompt that Claude Code shows at session startup
/// (for Opus / `--permission-mode auto` workers) fires *before*
/// `SessionStart`, so no hook ever arrives and the slot stays in `Spawning`
/// with `last_event_at = None`. `mark_stalled_spawns` must detect this and
/// flip the slot to `WaitingForInput` so the kanban dot + indicator fire.
#[test]
fn stalled_spawn_with_no_events_transitions_to_waiting_for_input() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);

    // Backdate the spawn time so the threshold has elapsed.
    let old_spawn = 1_700_000_000_i64;
    reg.set_spawn_time_for_test(1, old_spawn);

    // No hooks have arrived — last_event_at is None, activity is Spawning.
    let before = reg.get(1).unwrap();
    assert_eq!(before.activity, WorkerActivity::Spawning);
    assert!(before.last_event_at.is_none());

    let now = old_spawn + STALLED_SPAWN_THRESHOLD_SECS + 1;
    let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

    assert_eq!(changed, vec![1], "slot 1 should be reported as changed");
    let after = reg.get(1).unwrap();
    assert_eq!(after.activity, WorkerActivity::WaitingForInput);
    assert!(
        after.last_event_at.is_some(),
        "last_event_at must be stamped on the stall transition"
    );
}

/// A worker that received at least one hook event (even just `SessionStart`)
/// is NOT considered stalled, even if it is still in `Spawning` state
/// (which can't happen in practice, but is a meaningful boundary).
#[test]
fn spawn_with_events_is_not_marked_stalled() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(2, "run-2", "claude-opus-4-7", 1, None);

    // Fire SessionStart so last_event_at gets set.
    reg.apply_event(
        2,
        &WorkerEvent::SessionStart {
            session_id: "s".into(),
            source: SessionStartSource::Startup,
            model: None,
        },
    );

    // Backdate the spawn time.
    reg.set_spawn_time_for_test(2, 1_700_000_000);

    let now = 1_700_000_000 + STALLED_SPAWN_THRESHOLD_SECS + 100;
    let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

    assert!(changed.is_empty(), "slot with events must not be flagged");
    let state = reg.get(2).unwrap();
    assert_eq!(state.activity, WorkerActivity::Idle);
}

/// A worker that spawned very recently is not yet considered stalled —
/// it just needs more time to start.
#[test]
fn freshly_spawned_worker_not_marked_stalled() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(3, "run-3", "claude-opus-4-7", 1, None);

    // The spawn time is "now", so the threshold has not elapsed.
    let now = 1_700_000_100_i64;
    reg.set_spawn_time_for_test(3, now - STALLED_SPAWN_THRESHOLD_SECS + 5);

    let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

    assert!(changed.is_empty(), "freshly-spawned worker must not be flagged");
    assert_eq!(
        reg.get(3).unwrap().activity,
        WorkerActivity::Spawning,
        "activity must remain Spawning"
    );
}

/// Regression test for the 2026-07-03/04 false-live incident: a
/// slot that never reported a shell pid must NOT be promoted to
/// `WaitingForInput` by `mark_stalled_spawns`, no matter how long it
/// has been stuck in `Spawning`. Promoting it there previously left
/// the slot parked forever at `activity=waiting_for_input,
/// shell_pid=0` — a state with nothing for a human to attach to and
/// answer. This slot is instead left in `Spawning` for
/// `spawn_ack_sweep` to terminal-fail and redispatch.
#[test]
fn zero_pid_spawn_is_not_promoted_to_waiting_for_input() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 0, None);

    let old_spawn = 1_700_000_000_i64;
    reg.set_spawn_time_for_test(1, old_spawn);

    // Far past the threshold — if this were a shell_pid > 0 slot it
    // would have been promoted long ago.
    let now = old_spawn + STALLED_SPAWN_THRESHOLD_SECS * 10;
    let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

    assert!(
        changed.is_empty(),
        "a pid-less slot must never be promoted by this path"
    );
    let state = reg.get(1).unwrap();
    assert_eq!(
        state.activity,
        WorkerActivity::Spawning,
        "must remain Spawning — driver-start verification owns the pid-less timeout path"
    );
    assert_eq!(state.shell_pid, 0);
}

/// Mirrors `awaiting_input_incapable_driver_never_shows_waiting_for_input`
/// for the stalled-spawn path: a slot whose driver doesn't declare
/// `Capability::AwaitingInputSignal` must never be promoted to
/// `WaitingForInput` by `mark_stalled_spawns`, even after the threshold
/// elapses with `shell_pid > 0` and no hook event — exactly the shape
/// that promotes a capable slot. It must be left in `Spawning` instead
/// of the fabricated state.
#[test]
fn awaiting_input_incapable_driver_never_marked_stalled() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.set_awaiting_input_capable(1, false);

    let old_spawn = 1_700_000_000_i64;
    reg.set_spawn_time_for_test(1, old_spawn);

    let now = old_spawn + STALLED_SPAWN_THRESHOLD_SECS + 1;
    let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

    assert!(
        changed.is_empty(),
        "an awaiting-input-incapable driver's slot must never be marked stalled"
    );
    let state = reg.get(1).unwrap();
    assert_eq!(
        state.activity,
        WorkerActivity::Spawning,
        "must remain Spawning — no lower-fidelity fallback exists yet for this driver"
    );
}

/// Workers in non-Spawning states are never touched by `mark_stalled_spawns`.
#[test]
fn non_spawning_states_not_affected_by_stall_detection() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);

    // Advance to Working via PreToolUse.
    reg.apply_event(1, &pre_tool("Bash"));

    reg.set_spawn_time_for_test(1, 1_700_000_000);

    let now = 1_700_000_000 + STALLED_SPAWN_THRESHOLD_SECS + 100;
    let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

    assert!(changed.is_empty(), "Working slot must not be flagged");
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);
}

/// `mark_stalled_spawns` is idempotent: once a slot transitions to
/// `WaitingForInput`, it is no longer in `Spawning` and will not be
/// transitioned again.
#[test]
fn mark_stalled_spawns_is_idempotent() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.set_spawn_time_for_test(1, 1_700_000_000);

    let now = 1_700_000_000 + STALLED_SPAWN_THRESHOLD_SECS + 1;
    let first = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);
    assert_eq!(first, vec![1]);

    let second = reg.mark_stalled_spawns(now + 10, STALLED_SPAWN_THRESHOLD_SECS);
    assert!(second.is_empty(), "should not fire again after first transition");
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
}

#[test]
fn progress_fidelity_defaults_to_rich_when_never_set() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    assert_eq!(reg.progress_fidelity_for_slot(1), ProgressFidelity::Rich);
    // Even for a slot that was never registered at all.
    assert_eq!(reg.progress_fidelity_for_slot(9), ProgressFidelity::Rich);
}

#[test]
fn set_progress_fidelity_round_trips() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.set_progress_fidelity(1, ProgressFidelity::Coarse);
    assert_eq!(reg.progress_fidelity_for_slot(1), ProgressFidelity::Coarse);
}

/// The trace is only useful if the `registered`/`cleared` pair balances
/// per run id. A slot re-registered without an intervening
/// `release_slot` breaks that: the prior run leaves `agents list` with
/// no `cleared` line of its own. Pin that the displacement is greppable
/// — both as a dedicated `warn` naming the displaced run, and as a
/// `replaced_run_id` field on the registration line itself.
#[test]
fn register_spawn_traces_a_displaced_entry() {
    let buffer = crate::test_support::log_capture::install();
    let start = buffer.lock().len();

    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(7, "run-displaced-first", "claude-opus-4-7", 11, None);
    reg.register_spawn(7, "run-displaced-second", "claude-opus-4-7", 22, None);

    let captured = String::from_utf8(buffer.lock()[start..].to_vec()).expect("utf8 log capture");
    let ours: Vec<&str> = captured
        .lines()
        .filter(|line| line.contains("run-displaced-"))
        .collect();

    let warn = ours
        .iter()
        .find(|line| line.contains("displaced a live entry without a release_slot"))
        .unwrap_or_else(|| panic!("no displacement warning captured; lines: {ours:#?}"));
    assert!(warn.contains("WARN"), "displacement must be a warning: {warn}");
    assert!(
        warn.contains("run_id=run-displaced-first"),
        "the warning must name the run that lost its listing: {warn}"
    );
    assert!(
        warn.contains("replaced_by_run_id=run-displaced-second"),
        "the warning must name the run that took the slot: {warn}"
    );

    let second_registration = ours
        .iter()
        .find(|line| line.contains("run is now visible") && line.contains("run_id=run-displaced-second"))
        .unwrap_or_else(|| panic!("no registration line for the second run; lines: {ours:#?}"));
    assert!(
        second_registration.contains("replaced_run_id=\"run-displaced-first\""),
        "the registration line must carry the displaced run id: {second_registration}"
    );

    // A registration onto an empty slot must not claim a displacement.
    let first_registration = ours
        .iter()
        .find(|line| line.contains("run is now visible") && line.contains("run_id=run-displaced-first"))
        .unwrap_or_else(|| panic!("no registration line for the first run; lines: {ours:#?}"));
    assert!(
        first_registration.contains("replaced_run_id=\"-\""),
        "a fresh slot must report no displacement: {first_registration}"
    );
}

#[test]
fn register_spawn_resets_fidelity_on_slot_recycle() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.set_progress_fidelity(1, ProgressFidelity::Minimal);
    assert_eq!(reg.progress_fidelity_for_slot(1), ProgressFidelity::Minimal);

    // Slot 1 is recycled for a new run — must not inherit the prior
    // occupant's declared tier.
    reg.register_spawn(1, "run-2", "claude-opus-4-7", 2, None);
    assert_eq!(reg.progress_fidelity_for_slot(1), ProgressFidelity::Rich);
}

#[test]
fn release_slot_clears_progress_fidelity() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.set_progress_fidelity(1, ProgressFidelity::Coarse);
    reg.release_slot(1);
    assert_eq!(reg.progress_fidelity_for_slot(1), ProgressFidelity::Rich);
}

// ── SessionStart model authority + stale-activity downgrade ──────────────

#[test]
fn session_start_model_overwrites_launch_default() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "opus", 1, None);
    assert_eq!(reg.get(1).unwrap().model, "opus");

    reg.apply_event(
        1,
        &WorkerEvent::SessionStart {
            session_id: "s".into(),
            source: SessionStartSource::Startup,
            model: Some("claude-opus-4-7".into()),
        },
    );
    assert_eq!(
        reg.get(1).unwrap().model,
        "claude-opus-4-7",
        "SessionStart model is authoritative over the launch default",
    );
}

#[test]
fn session_start_without_model_keeps_launch_default() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "opus", 1, None);
    reg.apply_event(
        1,
        &WorkerEvent::SessionStart {
            session_id: "s".into(),
            source: SessionStartSource::Startup,
            model: None,
        },
    );
    assert_eq!(
        reg.get(1).unwrap().model,
        "opus",
        "absent model must not wipe the launch default",
    );
}

#[test]
fn session_start_resume_stamps_model_but_leaves_spawning() {
    // Resume is the reattach proof-of-life path: stamp model + last_event_at
    // without claiming the worker is past spawn. The stale-activity timer
    // then downgrades Spawning → Idle once last_event_at ages out.
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "opus", 1, None);
    reg.apply_event(
        1,
        &WorkerEvent::SessionStart {
            session_id: "s".into(),
            source: SessionStartSource::Resume,
            model: Some("claude-sonnet-4-6".into()),
        },
    );
    let state = reg.get(1).unwrap();
    assert_eq!(state.model, "claude-sonnet-4-6");
    assert_eq!(state.activity, WorkerActivity::Spawning);
    assert!(state.last_event_at.is_some());
}

#[test]
fn stale_last_event_at_downgrades_spawning_to_idle() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    // Resume leaves Spawning while stamping last_event_at.
    reg.apply_event(
        1,
        &WorkerEvent::SessionStart {
            session_id: "s".into(),
            source: SessionStartSource::Resume,
            model: None,
        },
    );
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Spawning);

    // Age last_event_at past the downgrade threshold.
    let now = 1_700_000_100_i64;
    let stale_at = iso8601_utc(now - STALE_ACTIVITY_DOWNGRADE_SECS - 1);
    reg.set_last_event_at_for_test(1, stale_at);

    let changed = reg.downgrade_stale_activity(now, STALE_ACTIVITY_DOWNGRADE_SECS);
    assert_eq!(changed, vec![1]);
    assert_eq!(
        reg.get(1).unwrap().activity,
        WorkerActivity::Idle,
        "stale last_event_at while Spawning must not keep advertising spawning",
    );
}

#[test]
fn recent_last_event_at_does_not_downgrade_spawning() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.apply_event(
        1,
        &WorkerEvent::SessionStart {
            session_id: "s".into(),
            source: SessionStartSource::Resume,
            model: None,
        },
    );
    let now = 1_700_000_100_i64;
    reg.set_last_event_at_for_test(1, iso8601_utc(now - 5));

    let changed = reg.downgrade_stale_activity(now, STALE_ACTIVITY_DOWNGRADE_SECS);
    assert!(changed.is_empty());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Spawning);
}

#[test]
fn downgrade_stale_activity_ignores_slots_with_no_events() {
    // No last_event_at → mark_stalled_spawns / spawn_ack_sweep, not us.
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    let now = 1_700_000_100_i64;
    let changed = reg.downgrade_stale_activity(now, STALE_ACTIVITY_DOWNGRADE_SECS);
    assert!(changed.is_empty());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Spawning);
}

#[test]
fn downgrade_stale_activity_ignores_non_spawning() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
    reg.apply_event(1, &pre_tool("Bash"));
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);

    let now = 1_700_000_100_i64;
    reg.set_last_event_at_for_test(1, iso8601_utc(now - STALE_ACTIVITY_DOWNGRADE_SECS - 60));
    let changed = reg.downgrade_stale_activity(now, STALE_ACTIVITY_DOWNGRADE_SECS);
    assert!(changed.is_empty());
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);
}

#[path = "tests/live_worker_state_driver_signal_tests.rs"]
mod driver_signal_tests;

#[path = "tests/live_worker_state_semantic_progress_tests.rs"]
mod semantic_progress_tests;
