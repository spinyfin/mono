//! Driver-originated semantic progress, distinct from display timestamps.
//!
//! [`LiveWorkerState::last_event_at`] is a *display* field: engine inference
//! such as spawn-stall handling and `mark_errored` writes it with no worker
//! activity at all. Persisting that container would let an engine-generated
//! stamp masquerade as agent progress after a restart.
//!
//! The load-bearing property is the last **driver-originated** event time
//! plus a tri-state tool condition. Both are updated only from the shared
//! progress-ingress fan-out (hook callbacks and JSONL tails), never from
//! engine-synthesized live-state writes. `"unknown"` is a real state: it
//! must never be coerced to `"idle"`, and a legacy NULL row stays unknown
//! until a real driver event establishes otherwise.

use boss_protocol::WorkerEvent;

/// What the engine knows about whether a tool is in flight on a run.
///
/// Stored as a TEXT column (`in_flight` / `idle` / `unknown`). Unrecognised
/// or NULL values parse as [`Self::Unknown`], never as idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SemanticToolCondition {
    /// An unbalanced `PreToolUse` is in flight.
    InFlight,
    /// A driver event established that no tool is in flight (`PostToolUse`,
    /// `Stop`, or `UserPromptSubmit`).
    Idle,
    /// No driver event has established tool state. The default for a fresh
    /// run and for every legacy row that predates the checkpoint columns.
    #[default]
    Unknown,
}

impl SemanticToolCondition {
    /// Stable on-disk / query label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InFlight => "in_flight",
            Self::Idle => "idle",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a stored column. `None` (legacy NULL) and any unrecognised
    /// string are [`Self::Unknown`] — never idle.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("in_flight") => Self::InFlight,
            Some("idle") => Self::Idle,
            Some("unknown") | None => Self::Unknown,
            Some(_) => Self::Unknown,
        }
    }
}

/// One run's durable semantic-progress checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticProgressCheckpoint {
    /// ISO-8601 UTC timestamp of the last driver-originated event.
    pub progress_at: String,
    /// Tri-state tool condition established by that event (or still
    /// unknown, if the event was not a tool-state signal).
    pub tool_condition: SemanticToolCondition,
}

/// Advance the tri-state tool condition for a driver-originated event.
///
/// `SessionStart`, `SessionEnd`, and `Notification` update the progress
/// *time* but are not tool-state signals, so they leave `previous`
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

/// What tool-state `event` establishes on its own, independent of any prior
/// condition — `None` for an event that carries no tool-state signal
/// (`SessionStart`, `SessionEnd`, `Notification`), and the established
/// condition otherwise.
///
/// Split out from [`next_tool_condition`] so a per-event writer can decide
/// whether it needs to touch the stored condition at all *without* reading
/// the previous value back first: a `None` here means "leave the column
/// alone," which a caller can express directly in SQL instead of resolving
/// `previous` just to hand it back unchanged.
pub fn tool_condition_signal(event: &WorkerEvent) -> Option<SemanticToolCondition> {
    match event {
        WorkerEvent::PreToolUse { .. } => Some(SemanticToolCondition::InFlight),
        WorkerEvent::PostToolUse { .. } | WorkerEvent::UserPromptSubmit { .. } | WorkerEvent::Stop { .. } => {
            Some(SemanticToolCondition::Idle)
        }
        WorkerEvent::SessionStart { .. } | WorkerEvent::SessionEnd { .. } | WorkerEvent::Notification { .. } => None,
    }
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
}
