//! Boundary equivalence across hook vs stdout-JSONL transports.
//!
//! Two separate claims, deliberately not collapsed:
//!
//! 1. **TurnEnd parity** — both transports produce equal [`TurnEnd`] values
//!    via [`AgentDriver::turn_boundary`] (hook-fired `Stop` and native
//!    `turn.completed` → same `TurnEnd` after
//!    [`AgentDriver::normalize_progress_event`]).
//! 2. **Activity-machine completion** — the decoded [`WorkerEvent`] sequence
//!    from either transport, applied through
//!    [`LiveWorkerStateRegistry::apply_event`], leaves the slot
//!    [`WorkerActivity::Idle`]. The registry is driven by `WorkerEvent`s
//!    (including `WorkerEvent::Stop`); it is **not** fed `TurnEnd` values.
//!    `turn_boundary` is checked alongside as the driver-level completion
//!    signal the engine uses for turn-boundary detection.
//!
//! These tests do not claim that completion detection is implemented solely
//! as `turn_boundary → TurnEnd` with no raw `WorkerEvent::Stop` match
//! elsewhere in the engine — only that both transports agree on `TurnEnd`
//! and that the activity machine reaches Idle from the decoded event stream.

use boss_protocol::{StopReason, WorkerEvent};

use crate::conformance::fixtures::{
    CANONICAL_SESSION_ID, CLAUDE_HOOK_SESSION_JSONL, CODEX_STDOUT_SESSION_JSONL, codex_shaped_driver, decode_jsonl,
};
use boss_protocol::WorkerActivity;

use crate::driver::{AgentDriver, ClaudeDriver, TurnEnd};
use crate::live_worker_state::LiveWorkerStateRegistry;

fn last_turn_end(driver: &dyn AgentDriver, jsonl: &str) -> TurnEnd {
    let events = decode_jsonl(driver, jsonl);
    let mut last = None;
    for event in &events {
        if let Some(end) = driver.turn_boundary(event) {
            last = Some(end);
        }
    }
    last.expect("session fixture must contain exactly one turn boundary")
}

#[test]
fn hook_stop_and_native_turn_completed_produce_identical_turn_end() {
    // Claim 1: turn_boundary yields the same TurnEnd from either transport.
    let from_hooks = last_turn_end(&ClaudeDriver, CLAUDE_HOOK_SESSION_JSONL);
    let from_stdout = last_turn_end(&codex_shaped_driver(), CODEX_STDOUT_SESSION_JSONL);

    assert_eq!(
        from_hooks,
        TurnEnd {
            session_id: CANONICAL_SESSION_ID.to_owned(),
            reason: StopReason::Completed,
            continuation: false,
        },
    );
    assert_eq!(
        from_hooks, from_stdout,
        "TurnEnd from hook Stop and from native turn.completed must be identical",
    );
}

#[test]
fn decoded_event_sequence_drives_activity_machine_to_idle_from_either_source() {
    // Claim 2: apply_event is fed WorkerEvents (not TurnEnd). Both transports'
    // decoded sequences leave the live-worker slot Idle after the turn.
    let codex = codex_shaped_driver();
    let cases: [(&str, &dyn AgentDriver, &str); 2] = [
        ("hook", &ClaudeDriver, CLAUDE_HOOK_SESSION_JSONL),
        ("stdout-jsonl", &codex, CODEX_STDOUT_SESSION_JSONL),
    ];
    for (label, driver, jsonl) in cases {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-boundary", "model", 42, None);
        let events = decode_jsonl(driver, jsonl);
        assert!(!events.is_empty(), "{label}: fixture must produce events",);
        for event in &events {
            assert!(
                reg.apply_event(1, event),
                "{label}: apply_event must accept every decoded WorkerEvent",
            );
            // Driver-level turn_boundary is the completion signal for claim 1;
            // assert it only fires on Stop-shaped events, independent of the
            // registry path (which sees raw WorkerEvents).
            let is_boundary = driver.turn_boundary(event).is_some();
            match event {
                WorkerEvent::Stop { .. } => assert!(is_boundary, "{label}: Stop must be a turn boundary"),
                _ => assert!(!is_boundary, "{label}: non-Stop must not be a turn boundary"),
            }
        }
        let state = reg.get(1).expect("slot still registered");
        assert_eq!(
            state.activity,
            WorkerActivity::Idle,
            "{label}: completed turn must leave the slot Idle, got {:?}",
            state.activity,
        );
        assert!(
            state.current_tool.is_none(),
            "{label}: tool must be cleared at turn end",
        );
    }
}

#[test]
fn only_stop_shaped_events_are_turn_boundaries_on_either_driver() {
    let mid_turn = [
        WorkerEvent::SessionStart {
            session_id: CANONICAL_SESSION_ID.to_owned(),
            source: boss_protocol::SessionStartSource::Startup,
        },
        WorkerEvent::UserPromptSubmit {
            session_id: CANONICAL_SESSION_ID.to_owned(),
            prompt: String::new(),
        },
        WorkerEvent::PreToolUse {
            session_id: CANONICAL_SESSION_ID.to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({}),
        },
        WorkerEvent::PostToolUse {
            session_id: CANONICAL_SESSION_ID.to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
        },
        WorkerEvent::Notification {
            session_id: CANONICAL_SESSION_ID.to_owned(),
            message: "hi".to_owned(),
        },
        WorkerEvent::SessionEnd {
            session_id: CANONICAL_SESSION_ID.to_owned(),
            reason: "other".to_owned(),
        },
    ];
    for event in &mid_turn {
        assert!(
            ClaudeDriver.turn_boundary(event).is_none(),
            "Claude must not treat {event:?} as a turn boundary",
        );
        assert!(
            codex_shaped_driver().turn_boundary(event).is_none(),
            "Codex-shaped must not treat {event:?} as a turn boundary",
        );
    }
}
