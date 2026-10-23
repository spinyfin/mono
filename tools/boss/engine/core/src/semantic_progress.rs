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

/// What the engine knows about whether the worker's *turn* is open.
///
/// Distinct from [`SemanticToolCondition`], which answers a narrower
/// question — "is a tool call unbalanced right now?" — and which is `Idle`
/// for most of an ordinary turn: between one tool's `PostToolUse` and the
/// next tool's `PreToolUse`, while the model thinks, and for the whole of a
/// Codex background-terminal wait (its continuation polls are deliberately
/// not re-announced, so they produce no tool-boundary event at all). None of
/// those is "the worker has ended its turn", so the tool condition alone
/// cannot decide whether a re-adopted slot is busy.
///
/// Stored as a TEXT column (`in_turn` / `ended` / `unknown`). Unrecognised
/// or NULL values parse as [`Self::Unknown`], never as ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SemanticTurnCondition {
    /// The driver reported turn activity (`UserPromptSubmit`, `PreToolUse`,
    /// `PostToolUse`) and has not reported a turn boundary since. The worker
    /// is busy — whatever its tool condition says.
    InTurn,
    /// The driver reported a turn boundary (`Stop`) and nothing since: the
    /// worker is parked at its prompt.
    Ended,
    /// No driver event has established turn state. The default for a fresh
    /// run and for every row that predates the column.
    #[default]
    Unknown,
}

impl SemanticTurnCondition {
    /// Stable on-disk / query label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InTurn => "in_turn",
            Self::Ended => "ended",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a stored column. `None` (legacy NULL) and any unrecognised
    /// string are [`Self::Unknown`] — never ended.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("in_turn") => Self::InTurn,
            Some("ended") => Self::Ended,
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
    /// Tri-state turn condition established by that event (or still
    /// unknown, if the event was not a turn-state signal).
    pub turn_condition: SemanticTurnCondition,
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

/// What turn-state `event` establishes on its own — `None` for an event
/// that carries no turn-state signal, and the established condition
/// otherwise.
///
/// The three turn-activity events and `Stop` are exactly the events
/// `apply_event` in `live_worker_state.rs` reduces to `Working` and
/// `Idle`/`WaitingForInput` respectively, so these are the driver's own
/// turn signals, never a timer or an inference from silence. The stored
/// column is not written from this function directly: it is persisted from
/// the reducer's post-apply state, which also honours ingress ordering and
/// ignores subagent tool hooks. `SessionStart` (including a resume) does
/// not open a turn, `SessionEnd` does not close one (a `SessionEnd` with an
/// unbalanced tool is evidence the session did *not* end — see the
/// reducer), and `Notification` is not a turn signal for the same reason it
/// is not a tool signal above.
pub fn turn_condition_signal(event: &WorkerEvent) -> Option<SemanticTurnCondition> {
    match event {
        WorkerEvent::PreToolUse { .. } | WorkerEvent::PostToolUse { .. } | WorkerEvent::UserPromptSubmit { .. } => {
            Some(SemanticTurnCondition::InTurn)
        }
        WorkerEvent::Stop { .. } => Some(SemanticTurnCondition::Ended),
        WorkerEvent::SessionStart { .. } | WorkerEvent::SessionEnd { .. } | WorkerEvent::Notification { .. } => None,
    }
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

    /// The incident shape: a turn whose last tool-boundary event was a
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
