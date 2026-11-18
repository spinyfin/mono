//! Driver-originated semantic progress, distinct from display timestamps.
//!
//! [`LiveWorkerState::last_event_at`] is a *display* field: engine inference
//! such as spawn-stall handling and `mark_errored` writes it with no worker
//! activity at all. Persisting that container would let an engine-generated
//! stamp masquerade as agent progress after a restart.
//!
//! The load-bearing property is the last **driver-originated** event time
//! plus a tool-control condition. Both are updated only from the shared
//! progress-ingress fan-out (hook callbacks and JSONL tails), never from
//! engine-synthesized live-state writes. `"unknown"` is a real state: it
//! must never be coerced to `"idle"`, and a legacy NULL row stays unknown
//! until a real driver event establishes otherwise.

use boss_protocol::WorkerEvent;

pub use boss_engine_storage_values::{
    SemanticProgressCheckpoint, SemanticToolCondition, SemanticTurnCondition, tool_condition_signal,
    turn_condition_signal,
};

/// Advance the tool condition for a driver-originated event.
///
/// `SessionStart`, `SessionEnd`, and `Notification` are not tool-state
/// signals, so they leave `previous`
/// untouched — including [`SemanticToolCondition::Unknown`]. A real
/// tool-boundary event is what first establishes idle or in-flight on a
/// legacy-null row.
///
/// `Notification` is deliberately excluded from the idle arm even though a
/// *trusted* Notification can mean "awaiting input" for capability-declaring
/// drivers: `apply_event` in `live_worker_state.rs` only honours that signal
/// behind the `awaiting_input_capable` gate, and for drivers that don't
/// declare it (e.g. Codex) a Notification can arrive mid-tool — a
/// guard-trace replay, a command denial, an unobserved-command marker —
/// none of which balance an outstanding `PreToolUse`. Coercing every
/// Notification to idle here would durably assert "no tool in flight" from
/// a signal the in-memory path refuses to trust, breaking `InFlight`'s
/// invariant that an unbalanced `PreToolUse` is in flight.
pub fn next_tool_condition(event: &WorkerEvent, previous: SemanticToolCondition) -> SemanticToolCondition {
    tool_condition_signal(event).unwrap_or(previous)
}

/// Advance the tri-state turn condition for a driver-originated event; the
/// turn-state twin of [`next_tool_condition`].
pub fn next_turn_condition(event: &WorkerEvent, previous: SemanticTurnCondition) -> SemanticTurnCondition {
    turn_condition_signal(event).unwrap_or(previous)
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::{SessionStartSource, StopReason};

    fn session_start() -> WorkerEvent {
        WorkerEvent::SessionStart {
            session_id: "s".into(),
            source: SessionStartSource::Startup,
            model: None,
        }
    }

    fn pre_tool() -> WorkerEvent {
        WorkerEvent::PreToolUse {
            session_id: "s".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
        }
    }

    fn post_tool() -> WorkerEvent {
        WorkerEvent::PostToolUse {
            session_id: "s".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
            tool_response: serde_json::Value::Null,
        }
    }

    fn stop() -> WorkerEvent {
        WorkerEvent::Stop {
            session_id: "s".into(),
            stop_hook_active: false,
            stop_reason: StopReason::Completed,
        }
    }

    fn session_end() -> WorkerEvent {
        WorkerEvent::SessionEnd {
            session_id: "s".into(),
            reason: "clear".into(),
        }
    }

    #[test]
    fn unknown_is_never_coerced_to_idle_by_session_start_or_end() {
        assert_eq!(
            next_tool_condition(&session_start(), SemanticToolCondition::Unknown),
            SemanticToolCondition::Unknown,
        );
        assert_eq!(
            next_tool_condition(&session_end(), SemanticToolCondition::Unknown),
            SemanticToolCondition::Unknown,
        );
        assert_eq!(
            next_tool_condition(&session_end(), SemanticToolCondition::InFlight),
            SemanticToolCondition::InFlight,
            "SessionEnd must not clear an in-flight tool, matching apply_event",
        );
    }

    #[test]
    fn notification_does_not_clear_an_in_flight_tool() {
        let notification = WorkerEvent::Notification {
            session_id: "s".into(),
            message: "guard-trace replay".into(),
        };
        assert_eq!(
            next_tool_condition(&notification, SemanticToolCondition::InFlight),
            SemanticToolCondition::InFlight,
            "Notification must not be treated as a tool-boundary signal, matching apply_event",
        );
        assert_eq!(
            next_tool_condition(&notification, SemanticToolCondition::Unknown),
            SemanticToolCondition::Unknown,
        );
    }

    #[test]
    fn tool_boundaries_establish_real_state_from_unknown() {
        assert_eq!(
            next_tool_condition(&pre_tool(), SemanticToolCondition::Unknown),
            SemanticToolCondition::InFlight,
        );
        assert_eq!(
            next_tool_condition(&post_tool(), SemanticToolCondition::InFlight),
            SemanticToolCondition::Idle,
        );
        assert_eq!(
            next_tool_condition(&stop(), SemanticToolCondition::Unknown),
            SemanticToolCondition::Idle,
        );
    }

    #[test]
    fn parse_never_turns_null_or_garbage_into_idle() {
        assert_eq!(SemanticToolCondition::parse(None), SemanticToolCondition::Unknown);
        assert_eq!(
            SemanticToolCondition::parse(Some("unknown")),
            SemanticToolCondition::Unknown,
        );
        assert_eq!(
            SemanticToolCondition::parse(Some("bogus")),
            SemanticToolCondition::Unknown,
        );
        assert_eq!(SemanticToolCondition::parse(Some("idle")), SemanticToolCondition::Idle,);
        assert_eq!(
            SemanticToolCondition::parse(Some("in_flight")),
            SemanticToolCondition::InFlight,
        );
    }

    fn user_prompt() -> WorkerEvent {
        WorkerEvent::UserPromptSubmit {
            session_id: "s".into(),
            prompt: String::new(),
        }
    }

    /// A turn whose last tool-boundary event was a
    /// `PostToolUse` reads `Idle` on the tool axis, but the turn is still
    /// open until the driver says otherwise. A worker waiting on its own
    /// background command sits in exactly this state.
    #[test]
    fn a_completed_tool_leaves_the_turn_open() {
        let mut turn = SemanticTurnCondition::Unknown;
        let mut tool = SemanticToolCondition::Unknown;
        for event in [user_prompt(), pre_tool(), post_tool()] {
            turn = next_turn_condition(&event, turn);
            tool = next_tool_condition(&event, tool);
        }
        assert_eq!(
            tool,
            SemanticToolCondition::Idle,
            "no tool is in flight after PostToolUse"
        );
        assert_eq!(
            turn,
            SemanticTurnCondition::InTurn,
            "but the turn has not ended: only a Stop establishes that",
        );
        assert_eq!(next_turn_condition(&stop(), turn), SemanticTurnCondition::Ended);
    }

    #[test]
    fn turn_condition_ignores_the_same_non_signals_as_tool_condition() {
        let notification = WorkerEvent::Notification {
            session_id: "s".into(),
            message: "guard-trace replay".into(),
        };
        for previous in [
            SemanticTurnCondition::Unknown,
            SemanticTurnCondition::InTurn,
            SemanticTurnCondition::Ended,
        ] {
            for event in [session_start(), session_end(), notification.clone()] {
                assert_eq!(turn_condition_signal(&event), None);
                assert_eq!(next_turn_condition(&event, previous), previous);
            }
        }
    }

    #[test]
    fn turn_parse_never_turns_null_or_garbage_into_ended() {
        assert_eq!(SemanticTurnCondition::parse(None), SemanticTurnCondition::Unknown);
        assert_eq!(
            SemanticTurnCondition::parse(Some("bogus")),
            SemanticTurnCondition::Unknown
        );
        assert_eq!(
            SemanticTurnCondition::parse(Some("unknown")),
            SemanticTurnCondition::Unknown
        );
        assert_eq!(
            SemanticTurnCondition::parse(Some("in_turn")),
            SemanticTurnCondition::InTurn
        );
        assert_eq!(
            SemanticTurnCondition::parse(Some("ended")),
            SemanticTurnCondition::Ended
        );
        for condition in [
            SemanticTurnCondition::InTurn,
            SemanticTurnCondition::Ended,
            SemanticTurnCondition::Unknown,
        ] {
            assert_eq!(SemanticTurnCondition::parse(Some(condition.as_str())), condition);
        }
    }
}
