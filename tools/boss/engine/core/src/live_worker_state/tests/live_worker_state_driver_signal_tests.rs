use super::*;

// ─── driver-start verification ──────────────────────────────────────────

/// Register a slot aged past `DRIVER_START_GRACE_SECS`, with a live
/// foreground shell pid — the 2026-07-30 shape.
fn aged_slot_with_live_shell(reg: &LiveWorkerStateRegistry, slot: u8, run: &str, awaiting_input_capable: bool) {
    reg.register_spawn_with_capabilities(
        slot,
        run,
        "grok-4.6",
        92697,
        None,
        awaiting_input_capable,
        LiveSpawnRouting::none(),
    );
    reg.set_spawn_time_for_test(
        slot,
        boss_engine_utils::epoch_time::now_epoch_secs() - (DRIVER_START_GRACE_SECS + 60),
    );
}

#[test]
fn unverified_driver_starts_reports_a_pane_with_a_live_shell_and_no_driver() {
    let reg = LiveWorkerStateRegistry::new();
    aged_slot_with_live_shell(&reg, 1, "run-a", false);

    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let found = reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS);

    assert_eq!(found.len(), 1, "a live shell pid must not exempt the slot");
    assert_eq!(found[0].slot_id, 1);
    assert_eq!(found[0].run_id, "run-a");
    assert_eq!(found[0].shell_pid, 92697);
    assert_eq!(found[0].activity, WorkerActivity::Spawning);
    assert!(found[0].silent_secs >= DRIVER_START_GRACE_SECS);
}

#[test]
fn a_driver_signal_removes_the_slot_from_the_unverified_set() {
    let reg = LiveWorkerStateRegistry::new();
    aged_slot_with_live_shell(&reg, 1, "run-a", false);
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    assert_eq!(reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).len(), 1);

    assert_eq!(reg.record_driver_signal("run-a", DriverSignalKind::HookEvent), Some(1));

    assert!(
        reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).is_empty(),
        "a driver-originated signal is proof the driver started",
    );
}

#[test]
fn either_driver_signal_kind_counts_as_proof() {
    for kind in [DriverSignalKind::HookEvent, DriverSignalKind::TranscriptPath] {
        let reg = LiveWorkerStateRegistry::new();
        aged_slot_with_live_shell(&reg, 1, "run-a", false);
        assert_eq!(reg.record_driver_signal("run-a", kind), Some(1));
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        assert!(
            reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).is_empty(),
            "{kind:?} must count as driver-start proof",
        );
    }
}

/// The signal is first-write-wins: it answers "did the driver ever
/// start?", not "when was it last alive". Later signals must not move it.
#[test]
fn record_driver_signal_keeps_the_first_timestamp() {
    let reg = LiveWorkerStateRegistry::new();
    aged_slot_with_live_shell(&reg, 1, "run-a", false);
    reg.record_driver_signal("run-a", DriverSignalKind::HookEvent);
    let first = reg.driver_signal_at(1).unwrap();
    reg.record_driver_signal("run-a", DriverSignalKind::TranscriptPath);
    assert_eq!(reg.driver_signal_at(1), Some(first));
}

/// Register a re-adopted slot aged past `DRIVER_START_GRACE_SECS` —
/// the shape re-adoption always produces, because registration stamps
/// `spawned_at` with the current time for a process that has in fact
/// been running for however long.
fn aged_readopted_slot(reg: &LiveWorkerStateRegistry, slot: u8, run: &str, evidence: ReadoptionEvidence) {
    reg.register_readoption(
        slot,
        run,
        "grok-4.6",
        92697,
        None,
        false,
        LiveSpawnRouting::none(),
        evidence,
    );
    reg.set_spawn_time_for_test(
        slot,
        boss_engine_utils::epoch_time::now_epoch_secs() - (DRIVER_START_GRACE_SECS + 60),
    );
}

/// A worker re-adopted on a live shell pid alone has no driver-start
/// proof. The fresh re-adoption grace window gives durable progress a
/// chance to restore proof after restart; after that, the slot must be
/// reported rather than permanently exempted by the login shell.
#[test]
fn a_live_shell_readoption_is_reported_when_its_driver_never_signalled() {
    let reg = LiveWorkerStateRegistry::new();
    aged_readopted_slot(&reg, 1, "run-a", ReadoptionEvidence::LiveShellPid);

    assert_eq!(reg.driver_start_expectation(1), Some(DriverStartExpectation::Readopted));
    assert!(
        reg.driver_signal_at(1).is_none(),
        "a live shell pid is not driver-start proof and must not be recorded as any",
    );

    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let found = reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].run_id, "run-a");
}

/// A hook arriving after the engine terminalized the run came from the
/// driver itself, so re-adoption records it rather than discarding it.
#[test]
fn a_hook_triggered_readoption_records_the_driver_signal() {
    let reg = LiveWorkerStateRegistry::new();
    aged_readopted_slot(&reg, 1, "run-a", ReadoptionEvidence::DriverHook);

    assert!(
        reg.driver_signal_at(1).is_some(),
        "the hook that triggered the re-adoption IS driver-originated proof",
    );
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    assert!(reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).is_empty());
}

#[test]
fn readopting_the_same_run_preserves_its_live_state_and_hold() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-a", "claude-opus-4-7", 0, None);
    reg.apply_event(1, &pre_tool("Bash"));
    reg.set_live_status(1, Some("editing the worker registry".into()));
    assert_eq!(reg.set_held("run-a", true), Some(1));
    let before = reg.get(1).unwrap();

    reg.register_readoption(
        1,
        "run-a",
        "opus",
        456,
        None,
        false,
        LiveSpawnRouting::none(),
        ReadoptionEvidence::LiveShellPid,
    );

    let repaired = reg.get(1).unwrap();
    assert_eq!(
        repaired.shell_pid, 456,
        "a positive observation repairs a provisional pid"
    );
    assert_eq!(repaired.run_id, before.run_id);
    assert_eq!(repaired.activity, before.activity);
    assert_eq!(repaired.held, before.held);
    assert_eq!(repaired.last_event_at, before.last_event_at);
    assert_eq!(reg.driver_start_expectation(1), Some(DriverStartExpectation::Readopted));

    reg.register_readoption(
        1,
        "run-a",
        "opus",
        0,
        None,
        false,
        LiveSpawnRouting::none(),
        ReadoptionEvidence::LiveShellPid,
    );
    assert_eq!(
        reg.get(1).unwrap(),
        repaired,
        "a pid-less re-adoption must not clobber the repaired shell pid or retained live state",
    );
}

#[test]
fn readopting_a_new_run_replaces_the_previous_occupants_live_state() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-a", "claude-opus-4-7", 123, None);
    reg.apply_event(1, &pre_tool("Bash"));
    assert_eq!(reg.set_held("run-a", true), Some(1));

    reg.register_readoption(
        1,
        "run-b",
        "opus",
        456,
        None,
        false,
        LiveSpawnRouting::none(),
        ReadoptionEvidence::LiveShellPid,
    );

    let state = reg.get(1).unwrap();
    assert_eq!(state.run_id, "run-b");
    assert_eq!(state.activity, WorkerActivity::Spawning);
    assert!(!state.held);
    assert!(state.last_event_at.is_none());
    assert_eq!(reg.driver_start_expectation(1), Some(DriverStartExpectation::Readopted));
}

/// The registration, not the slot, carries the expectation: recycling
/// the slot for a genuine spawn must restore `EngineSpawned` so pass
/// 1's spawn-ack timeout applies again.
#[test]
fn recycling_a_readopted_slot_for_a_real_spawn_restores_the_engine_spawned_expectation() {
    let reg = LiveWorkerStateRegistry::new();
    aged_readopted_slot(&reg, 1, "run-a", ReadoptionEvidence::LiveShellPid);

    aged_slot_with_live_shell(&reg, 1, "run-b", false);

    assert_eq!(
        reg.driver_start_expectation(1),
        Some(DriverStartExpectation::EngineSpawned)
    );
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let found = reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].run_id, "run-b");
}

#[test]
fn record_driver_signal_is_a_no_op_for_an_unknown_run() {
    let reg = LiveWorkerStateRegistry::new();
    aged_slot_with_live_shell(&reg, 1, "run-a", false);
    assert_eq!(reg.record_driver_signal("run-other", DriverSignalKind::HookEvent), None);
    assert!(reg.driver_signal_at(1).is_none());
}

/// The reconciliation that closes the grok hole: `mark_stalled_spawns`
/// declines to promote a driver without `Capability::AwaitingInputSignal`,
/// and that exemption must not carry over to driver-start verification.
#[test]
fn mark_stalled_spawns_capability_exemption_does_not_extend_to_driver_start() {
    let reg = LiveWorkerStateRegistry::new();
    aged_slot_with_live_shell(&reg, 1, "run-a", false);

    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    assert!(
        reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS).is_empty(),
        "precondition: the capability-less driver is exempt from promotion",
    );
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Spawning);

    assert_eq!(
        reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).len(),
        1,
        "driver-start verification must cover the driver mark_stalled_spawns skips",
    );
}

/// Closes the gap this exemption otherwise leaves open: a capability-less
/// driver (Codex, Grok) with real driver-originated evidence —
/// `driver_signal_at`, e.g. from `record_driver_attach` observing the
/// rollout file — must still leave `Spawning`, just onto `Idle` rather than
/// the `WaitingForInput` guess this driver class gives no basis for.
#[test]
fn mark_stalled_spawns_promotes_to_idle_on_driver_signal_without_capability() {
    let reg = LiveWorkerStateRegistry::new();
    aged_slot_with_live_shell(&reg, 1, "run-a", false);
    assert_eq!(
        reg.record_driver_signal("run-a", DriverSignalKind::TranscriptPath),
        Some(1)
    );

    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    assert_eq!(reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS), vec![1]);
    let state = reg.get(1).unwrap();
    assert_eq!(state.activity, WorkerActivity::Idle);
    assert!(state.last_event_at.is_some());
}

/// The promotion path's synthesized `last_event_at` must not be mistaken
/// for driver evidence, and leaving `Spawning` must not hide the slot.
#[test]
fn mark_stalled_spawns_promotion_is_not_driver_evidence() {
    let reg = LiveWorkerStateRegistry::new();
    aged_slot_with_live_shell(&reg, 1, "run-a", true);

    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    assert_eq!(reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS), vec![1]);
    let state = reg.get(1).unwrap();
    assert_eq!(state.activity, WorkerActivity::WaitingForInput);
    assert!(state.last_event_at.is_some());

    assert!(
        reg.driver_signal_at(1).is_none(),
        "an engine-synthesized timestamp is not a driver signal",
    );
    let found = reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS);
    assert_eq!(found.len(), 1, "a promoted slot stays subject to verification");
    assert_eq!(found[0].activity, WorkerActivity::WaitingForInput);
}

#[test]
fn unverified_driver_starts_respects_the_grace_window() {
    let reg = LiveWorkerStateRegistry::new();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    reg.register_spawn(1, "run-a", "grok-4.6", 92697, None);
    reg.set_spawn_time_for_test(1, now - 5);

    assert!(
        reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).is_empty(),
        "a fresh spawn must be given its window before being judged",
    );
}

/// A recycled slot must not inherit the previous occupant's driver-start
/// proof — otherwise a healthy prior run would vouch for a new run whose
/// driver never exec'd.
#[test]
fn re_registering_a_slot_clears_the_prior_driver_signal() {
    let reg = LiveWorkerStateRegistry::new();
    aged_slot_with_live_shell(&reg, 1, "run-a", false);
    reg.record_driver_signal("run-a", DriverSignalKind::HookEvent);
    assert!(reg.driver_signal_at(1).is_some());

    aged_slot_with_live_shell(&reg, 1, "run-b", false);

    assert!(reg.driver_signal_at(1).is_none());
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let found = reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].run_id, "run-b");
}

#[test]
fn releasing_a_slot_clears_its_driver_signal() {
    let reg = LiveWorkerStateRegistry::new();
    aged_slot_with_live_shell(&reg, 1, "run-a", false);
    reg.record_driver_signal("run-a", DriverSignalKind::HookEvent);
    reg.release_slot(1);
    assert!(reg.driver_signal_at(1).is_none());
}

/// A real hook through the normal `apply_event` path does NOT by itself
/// stamp the driver signal — the hook ingress records it explicitly. This
/// pins that the two are separate concerns so a future refactor of
/// `apply_event` cannot silently start (or stop) vouching for a driver.
#[test]
fn apply_event_alone_does_not_stamp_the_driver_signal() {
    let reg = LiveWorkerStateRegistry::new();
    aged_slot_with_live_shell(&reg, 1, "run-a", false);
    reg.apply_event(1, &pre_tool("Bash"));
    assert!(reg.get(1).unwrap().last_event_at.is_some());
    assert!(reg.driver_signal_at(1).is_none());
}
