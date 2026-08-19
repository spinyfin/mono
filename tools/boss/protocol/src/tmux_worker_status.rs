//! On-demand tmux evidence for a live worker.
//!
//! Unlike [`crate::LiveWorkerState`], this is not broadcast on every hook.
//! Reading tmux is an external observation, so the engine only collects it
//! when an operator asks for `bossctl agents list`.

use serde::{Deserialize, Serialize};

/// Whether the live tmux session can currently be adopted as the durable run
/// recorded for an execution.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TmuxAdoptionState {
    /// The execution has no durable tmux identity (for example, a remote or
    /// legacy app-hosted worker).
    NotTmuxHosted,
    /// The session exists and its authoritative spawn token matches the
    /// durable identity. This is the only state that is safe to adopt.
    Adopted,
    /// The durable session name is no longer present on the private server.
    SessionMissing,
    /// A session exists at the durable name, but its token identifies a
    /// different worker and must never be adopted or destroyed by this run.
    TokenMismatch,
    /// tmux could not be queried, so adoption cannot be evaluated.
    ProbeUnavailable,
}

/// Tmux liveness evidence associated with one live worker execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TmuxWorkerStatus {
    /// The execution (`exec_*`) that owns this status row.
    pub execution_id: String,
    /// Human-readable tmux session name. It is display-only; durable token
    /// matching remains the identity check.
    pub session_name: Option<String>,
    pub adoption_state: TmuxAdoptionState,
    /// `#{pane_dead}` when the session can be read. `None` means tmux could
    /// not establish a comparable live pane.
    pub pane_dead: Option<bool>,
    /// ISO-8601 rendering of tmux's `#{window_activity}` timestamp, when
    /// available. This advances while detached and is the last output time.
    pub last_output_at: Option<String>,
}
