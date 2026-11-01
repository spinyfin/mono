/// What the engine knows about whether a tool is in flight on a run.
///
/// Stored as a TEXT column (`in_flight` / `yielded` / `idle` / `unknown`). Unrecognised
/// or NULL values parse as [`Self::Unknown`], never as idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SemanticToolCondition {
    /// An unbalanced `PreToolUse` is in flight.
    InFlight,
    /// A command remains running, but its yield returned control to the model.
    Yielded,
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
            Self::Yielded => "yielded",
            Self::Idle => "idle",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a stored column. `None` (legacy NULL) and any unrecognised
    /// string are [`Self::Unknown`] — never idle.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("in_flight") => Self::InFlight,
            Some("yielded") => Self::Yielded,
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
/// next tool's `PreToolUse`, while the model thinks. Codex background commands
/// are `InFlight` while a poll blocks and `Yielded` after its result returns
/// control to the model without balancing the command. None of
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
    /// Tool condition established by that event (or still
    /// unknown, if the event was not a tool-state signal).
    pub tool_condition: SemanticToolCondition,
    /// Tri-state turn condition established by that event (or still
    /// unknown, if the event was not a turn-state signal).
    pub turn_condition: SemanticTurnCondition,
}

use boss_protocol::WorkerEvent;

/// What tool-state `event` establishes on its own, independent of any prior
/// condition — `None` for an event that carries no tool-state signal.
pub fn tool_condition_signal(event: &WorkerEvent) -> Option<SemanticToolCondition> {
    match event {
        WorkerEvent::ToolPoll { awaiting_model, .. } => Some(if *awaiting_model {
            SemanticToolCondition::Yielded
        } else {
            SemanticToolCondition::InFlight
        }),
        WorkerEvent::PreToolUse { .. } => Some(SemanticToolCondition::InFlight),
        WorkerEvent::PostToolUse { .. } | WorkerEvent::UserPromptSubmit { .. } | WorkerEvent::Stop { .. } => {
            Some(SemanticToolCondition::Idle)
        }
        WorkerEvent::ModelProgress { .. }
        | WorkerEvent::SessionStart { .. }
        | WorkerEvent::SessionEnd { .. }
        | WorkerEvent::Notification { .. } => None,
    }
}

/// What turn-state `event` establishes on its own — `None` for an event
/// that carries no turn-state signal.
pub fn turn_condition_signal(event: &WorkerEvent) -> Option<SemanticTurnCondition> {
    match event {
        WorkerEvent::PreToolUse { .. } | WorkerEvent::PostToolUse { .. } | WorkerEvent::UserPromptSubmit { .. } => {
            Some(SemanticTurnCondition::InTurn)
        }
        WorkerEvent::Stop { .. } => Some(SemanticTurnCondition::Ended),
        WorkerEvent::ToolPoll { .. }
        | WorkerEvent::ModelProgress { .. }
        | WorkerEvent::SessionStart { .. }
        | WorkerEvent::SessionEnd { .. }
        | WorkerEvent::Notification { .. } => None,
    }
}
