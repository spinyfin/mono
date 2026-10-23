use super::*;
use crate::semantic_progress::{SemanticProgressCheckpoint, SemanticToolCondition, SemanticTurnCondition};

#[test]
fn seed_semantic_progress_restores_in_flight_without_display_timestamp() {
    let reg = LiveWorkerStateRegistry::new();
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
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Spawning);
    assert!(reg.get(1).unwrap().last_event_at.is_none());

    reg.seed_semantic_progress(
        1,
        &SemanticProgressCheckpoint {
            progress_at: "2026-09-02T12:00:00Z".into(),
            tool_condition: SemanticToolCondition::InFlight,
            turn_condition: crate::semantic_progress::SemanticTurnCondition::Unknown,
        },
    );

    let state = reg.get(1).unwrap();
    assert_eq!(state.activity, WorkerActivity::Working);
    assert!(
        state.last_event_at.is_none(),
        "seeding must not treat the checkpoint as a display timestamp",
    );
    let seeded = reg.semantic_progress_for_slot(1).unwrap();
    assert_eq!(seeded.progress_at, "2026-09-02T12:00:00Z");
    assert_eq!(seeded.tool_condition, SemanticToolCondition::InFlight);
    assert!(
        reg.driver_signal_at(1).is_some(),
        "a durable checkpoint proves this re-adopted driver signalled before restart",
    );
    assert!(
        reg.unverified_driver_starts(
            boss_engine_utils::epoch_time::now_epoch_secs() + DRIVER_START_GRACE_SECS + 60,
            DRIVER_START_GRACE_SECS,
        )
        .is_empty(),
        "restored driver proof must keep a re-adopted worker out of the timeout set",
    );
}

#[test]
fn seed_semantic_progress_leaves_unknown_as_spawning() {
    let reg = LiveWorkerStateRegistry::new();
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
    reg.seed_semantic_progress(
        1,
        &SemanticProgressCheckpoint {
            progress_at: "2026-09-02T12:00:00Z".into(),
            tool_condition: SemanticToolCondition::Unknown,
            turn_condition: crate::semantic_progress::SemanticTurnCondition::Unknown,
        },
    );
    assert_eq!(
        reg.get(1).unwrap().activity,
        WorkerActivity::Spawning,
        "unknown must never be coerced to idle",
    );
}

#[test]
fn seed_semantic_progress_does_not_overwrite_hook_derived_state() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-a", "claude-opus-4-7", 123, None);
    reg.apply_event(1, &pre_tool("Bash"));
    let before = reg.get(1).unwrap();

    reg.seed_semantic_progress(
        1,
        &SemanticProgressCheckpoint {
            progress_at: "2026-01-01T00:00:00Z".into(),
            tool_condition: SemanticToolCondition::Idle,
            turn_condition: crate::semantic_progress::SemanticTurnCondition::Unknown,
        },
    );

    let after = reg.get(1).unwrap();
    assert_eq!(after.activity, before.activity);
    assert_eq!(after.last_event_at, before.last_event_at);
    assert_eq!(
        reg.semantic_progress_for_slot(1).unwrap().tool_condition,
        SemanticToolCondition::InFlight,
        "in-memory driver progress must win over a stale checkpoint",
    );
}

fn readopted(reg: &LiveWorkerStateRegistry) {
    reg.register_readoption(
        1,
        "run-a",
        "codex",
        456,
        None,
        false,
        LiveSpawnRouting::none(),
        ReadoptionEvidence::LiveShellPid,
    );
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Spawning);
}

/// The 2026-10-23 incident shape. A Codex worker mid-turn, waiting on its own
/// background `checkleft run`: its last tool-boundary event was the
/// `PostToolUse` of an earlier command (tool condition `Idle`), its
/// continuation polls produce no events, and the turn has not ended. The
/// engine restarts and re-adopts the slot from this checkpoint. The worker
/// is busy, and the seeded slot must say so.
#[test]
fn seed_semantic_progress_restores_working_for_an_open_turn_with_no_tool_in_flight() {
    let reg = LiveWorkerStateRegistry::new();
    readopted(&reg);

    reg.seed_semantic_progress(
        1,
        &SemanticProgressCheckpoint {
            progress_at: "2026-10-23T19:27:22Z".into(),
            tool_condition: SemanticToolCondition::Idle,
            turn_condition: SemanticTurnCondition::InTurn,
        },
    );

    let state = reg.get(1).unwrap();
    assert_eq!(
        state.activity,
        WorkerActivity::Working,
        "a turn the driver has not ended is busy, whatever the tool condition says",
    );
    assert_eq!(state.current_tool, None, "no tool is in flight, and none is fabricated");
    let seeded = reg.semantic_progress_for_slot(1).unwrap();
    assert_eq!(seeded.tool_condition, SemanticToolCondition::Idle);
    assert_eq!(seeded.turn_condition, SemanticTurnCondition::InTurn);
}

#[test]
fn seed_semantic_progress_restores_idle_only_for_a_turn_the_driver_ended() {
    let reg = LiveWorkerStateRegistry::new();
    readopted(&reg);

    reg.seed_semantic_progress(
        1,
        &SemanticProgressCheckpoint {
            progress_at: "2026-10-23T19:27:22Z".into(),
            tool_condition: SemanticToolCondition::Idle,
            turn_condition: SemanticTurnCondition::Ended,
        },
    );

    assert_eq!(
        reg.get(1).unwrap().activity,
        WorkerActivity::Idle,
        "a Stop is the one signal that parks a worker at its prompt",
    );
}

#[test]
fn seed_semantic_progress_keeps_the_pre_column_mapping_for_a_legacy_row() {
    // A row written before `semantic_turn_condition` existed: turn
    // unknown. An unbalanced tool is still in-turn evidence; a balanced one
    // keeps the old idle reading rather than guessing, and the re-established
    // ingress corrects it on the next event.
    for (tool_condition, expected) in [
        (SemanticToolCondition::InFlight, WorkerActivity::Working),
        (SemanticToolCondition::Idle, WorkerActivity::Idle),
        (SemanticToolCondition::Unknown, WorkerActivity::Spawning),
    ] {
        let reg = LiveWorkerStateRegistry::new();
        readopted(&reg);
        reg.seed_semantic_progress(
            1,
            &SemanticProgressCheckpoint {
                progress_at: "2026-09-02T12:00:00Z".into(),
                tool_condition,
                turn_condition: SemanticTurnCondition::Unknown,
            },
        );
        assert_eq!(
            reg.get(1).unwrap().activity,
            expected,
            "tool condition {tool_condition:?}"
        );
    }
}

/// The in-memory twin of the durable column: the reducer's own events keep
/// the turn open across a balanced tool call and close it on `Stop`, so what
/// the checkpoint later restores is what the slot itself believed.
#[test]
fn apply_event_tracks_the_turn_condition_alongside_the_tool_condition() {
    let reg = LiveWorkerStateRegistry::new();
    reg.register_spawn(1, "run-a", "codex", 123, None);
    assert_eq!(
        reg.semantic_progress_for_slot(1),
        None,
        "no driver event yet: no checkpoint to restore from",
    );

    reg.apply_event(1, &pre_tool("Bash"));
    let after_pre = reg.semantic_progress_for_slot(1).unwrap();
    assert_eq!(after_pre.tool_condition, SemanticToolCondition::InFlight);
    assert_eq!(after_pre.turn_condition, SemanticTurnCondition::InTurn);

    reg.apply_event(1, &post_tool("Bash"));
    let after_post = reg.semantic_progress_for_slot(1).unwrap();
    assert_eq!(after_post.tool_condition, SemanticToolCondition::Idle);
    assert_eq!(
        after_post.turn_condition,
        SemanticTurnCondition::InTurn,
        "a balanced tool call does not end the turn",
    );
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);

    reg.apply_event(1, &stop_event());
    let after_stop = reg.semantic_progress_for_slot(1).unwrap();
    assert_eq!(after_stop.turn_condition, SemanticTurnCondition::Ended);
    assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Idle);
}
