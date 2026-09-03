use super::*;
use crate::semantic_progress::{SemanticProgressCheckpoint, SemanticToolCondition};

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
