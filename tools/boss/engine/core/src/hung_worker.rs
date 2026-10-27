//! Non-destructive warning for a silent driver between tool calls.
//!
//! Thresholds come from the October 2026 driver timing sample documented in
//! worker-liveness-contract.md. Recovered stalls exist, so this never reaps.

use crate::driver::ProgressFidelity;
use crate::semantic_progress::{SemanticProgressCheckpoint, SemanticToolCondition, SemanticTurnCondition};
use boss_protocol::{LiveWorkerState, WorkerActivity};

pub(crate) const HUNG_WORKER_ATTENTION_KIND: &str = "hung_worker";

pub(crate) const FIRST_RESPONSE_SECS: i64 = 600;
pub(crate) const NEXT_RESPONSE_SECS: i64 = 900;

pub(crate) fn hung_since(
    state: &LiveWorkerState,
    checkpoint: Option<&SemanticProgressCheckpoint>,
    fidelity: ProgressFidelity,
    held: bool,
    now: i64,
) -> Option<String> {
    let checkpoint = checkpoint?;
    let yielded = checkpoint.tool_condition == SemanticToolCondition::Yielded;
    if held
        || state.wait_reason.is_some()
        || (state.current_tool.is_some() && !yielded)
        || state.activity != WorkerActivity::Working
    {
        return None;
    }
    if checkpoint.turn_condition != SemanticTurnCondition::InTurn {
        return None;
    }
    let threshold = if yielded || state.last_tool_ended_at.is_some() {
        NEXT_RESPONSE_SECS
    } else {
        FIRST_RESPONSE_SECS
    };
    // Yielded commands stay protected from destructive stale-worker reaping.
    // For this warning only, control has returned to the model. The shared
    // classifier uses a strict cutoff; subtract one second for this inclusive boundary.
    match crate::stale_worker_sweep::classify_semantic_staleness(
        if yielded {
            SemanticToolCondition::Idle
        } else {
            checkpoint.tool_condition
        },
        Some(&checkpoint.progress_at),
        &checkpoint.progress_at,
        fidelity,
        now,
        threshold - 1,
    ) {
        crate::stale_worker_sweep::SemanticStaleness::Stale { progress_at } => Some(progress_at),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_prompt_resets_the_completed_tool_threshold() {
        use crate::live_worker_state::LiveWorkerStateRegistry;
        use boss_protocol::WorkerEvent;
        let registry = LiveWorkerStateRegistry::new();
        registry.register_spawn(1, "run", "codex", 0, None);
        registry.apply_event(
            1,
            &WorkerEvent::PostToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::Value::Null,
                tool_response: serde_json::Value::Null,
            },
        );
        assert!(registry.get(1).unwrap().last_tool_ended_at.is_some());
        registry.apply_event(
            1,
            &WorkerEvent::UserPromptSubmit {
                session_id: "s".into(),
                prompt: "next".into(),
            },
        );
        assert!(registry.get(1).unwrap().last_tool_ended_at.is_none());
    }

    #[test]
    fn yielded_result_waits_for_model_but_next_poll_is_exempt() {
        use crate::live_worker_state::LiveWorkerStateRegistry;
        use boss_protocol::WorkerEvent;
        let registry = LiveWorkerStateRegistry::new();
        registry.register_spawn(1, "run", "codex", 0, None);
        registry.apply_event(
            1,
            &WorkerEvent::PreToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::Value::Null,
            },
        );
        registry.apply_event(
            1,
            &WorkerEvent::ToolPoll {
                session_id: "s".into(),
                awaiting_model: true,
            },
        );
        let mut checkpoint = registry.semantic_progress_for_slot(1).unwrap();
        assert_eq!(checkpoint.tool_condition, SemanticToolCondition::Yielded);
        checkpoint.progress_at = crate::live_worker_state::iso8601_utc(1000);
        let now = 1000;
        assert!(
            hung_since(
                &registry.get(1).unwrap(),
                Some(&checkpoint),
                ProgressFidelity::Rich,
                false,
                now + 899
            )
            .is_none()
        );
        assert!(
            hung_since(
                &registry.get(1).unwrap(),
                Some(&checkpoint),
                ProgressFidelity::Rich,
                false,
                now + 900
            )
            .is_some()
        );
        registry.apply_event(
            1,
            &WorkerEvent::ToolPoll {
                session_id: "s".into(),
                awaiting_model: false,
            },
        );
        assert!(
            hung_since(
                &registry.get(1).unwrap(),
                registry.semantic_progress_for_slot(1).as_ref(),
                ProgressFidelity::Rich,
                false,
                now + 900
            )
            .is_none()
        );
    }

    #[test]
    fn actual_model_output_clears_a_warning_without_balancing_a_running_tool() {
        use crate::live_worker_state::LiveWorkerStateRegistry;
        use boss_protocol::WorkerEvent;
        let registry = LiveWorkerStateRegistry::new();
        registry.register_spawn(1, "run", "codex", 0, None);
        registry.seed_semantic_progress(
            1,
            &SemanticProgressCheckpoint {
                progress_at: crate::live_worker_state::iso8601_utc(1000),
                tool_condition: SemanticToolCondition::Idle,
                turn_condition: SemanticTurnCondition::InTurn,
            },
        );
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let classify = || {
            hung_since(
                &registry.get(1).unwrap(),
                registry.semantic_progress_for_slot(1).as_ref(),
                ProgressFidelity::Rich,
                false,
                now,
            )
        };
        assert!(classify().is_some());
        registry.apply_event(1, &WorkerEvent::ModelProgress { session_id: "s".into() });
        assert!(classify().is_none());
        registry.apply_event(
            1,
            &WorkerEvent::PreToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::Value::Null,
            },
        );
        registry.apply_event(1, &WorkerEvent::ModelProgress { session_id: "s".into() });
        assert_eq!(
            registry.semantic_progress_for_slot(1).unwrap().tool_condition,
            SemanticToolCondition::InFlight
        );
        assert_eq!(registry.get(1).unwrap().current_tool.as_deref(), Some("Bash"));
    }

    #[test]
    fn warns_at_the_measured_boundary_and_protects_tools_and_waits() {
        let mut state = LiveWorkerState::new_spawning(1, "run", "model", 0, None);
        state.activity = WorkerActivity::Working;
        let mut checkpoint = SemanticProgressCheckpoint {
            progress_at: crate::live_worker_state::iso8601_utc(1000),
            tool_condition: SemanticToolCondition::Idle,
            turn_condition: SemanticTurnCondition::InTurn,
        };
        let classify = |state: &LiveWorkerState, checkpoint: &SemanticProgressCheckpoint, now| {
            hung_since(state, Some(checkpoint), ProgressFidelity::Rich, false, now)
        };
        assert!(classify(&state, &checkpoint, 1599).is_none());
        assert!(classify(&state, &checkpoint, 1600).is_some());
        state.last_tool_ended_at = Some(checkpoint.progress_at.clone());
        assert!(classify(&state, &checkpoint, 1899).is_none());
        assert!(classify(&state, &checkpoint, 1900).is_some());
        checkpoint.tool_condition = SemanticToolCondition::InFlight;
        assert!(classify(&state, &checkpoint, 10000).is_none());
        checkpoint.tool_condition = SemanticToolCondition::Idle;
        state.wait_reason = Some("long build".into());
        assert!(classify(&state, &checkpoint, 10000).is_none());
        state.wait_reason = None;
        assert!(hung_since(&state, Some(&checkpoint), ProgressFidelity::Rich, true, 10000).is_none());
        checkpoint.progress_at = crate::live_worker_state::iso8601_utc(9999);
        assert!(classify(&state, &checkpoint, 10000).is_none());
        checkpoint.turn_condition = SemanticTurnCondition::Ended;
        assert!(classify(&state, &checkpoint, 20000).is_none());
    }
}
