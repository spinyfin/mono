//! Durable-state classification of every worker pane the app hosts,
//! whether or not the engine's in-memory `LiveWorkerStateRegistry` still
//! tracks it.
//!
//! Backs `bossctl agents list --all` and worker-reference resolution
//! (crew name / slot id / run id) for every `agents` verb: a name or slot
//! visible in the app must resolve even after the engine drops the live
//! registry entry (crash, terminal-fail path, spawn-ack timeout). See
//! `boss_engine::app::pane_ops::ServerState::list_hosted_pane_statuses`
//! for how this is computed, and [`crate::HostedPaneEntry`] for the raw
//! (unclassified) app report this is derived from.

use serde::{Deserialize, Serialize};

/// Where a hosted pane sits relative to the engine's live-worker
/// registry and durable state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum HostedPaneState {
    /// The engine's `LiveWorkerStateRegistry` has a live (non-terminal)
    /// entry for this run — an ordinary, actively-tracked worker.
    Live,
    /// No live registry entry (or only a terminal one), but durable
    /// state — `work_runs.shell_pid` plus the execution's own row —
    /// corroborates a still-running worker process. This is the shape a
    /// worker the engine has lost track of takes: durably tracked, not
    /// live-tracked. `evidence` names the corroborating signal.
    LiveProcessNoRegistry { evidence: String },
    /// No live registry entry and no corroborated live process — a true
    /// husk, safe to retire.
    Husk,
}

/// One slot the app reports hosting a session in, classified against
/// the engine's live registry and durable state. The CLI-facing,
/// classified counterpart of [`crate::HostedPaneEntry`] (which carries
/// only what the app itself knows, with no opinion on liveness).
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[builder(on(String, into))]
pub struct HostedPaneStatus {
    pub slot_id: u8,
    pub run_id: String,
    /// Derived from `slot_id` via `worker_names::name_for_slot` — the
    /// same crew name the app renders in the pane header, recoverable
    /// purely from the slot number regardless of whether the live
    /// registry still has an entry.
    pub crew_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_title: Option<String>,
    pub state: HostedPaneState,
}
