//! Why a [`crate::FrontendRequest::RecreateCoordinator`] was confirmed.
//!
//! Split out of `wire.rs` (and re-exported from it) to keep that module
//! under the file-size limit.

use serde::{Deserialize, Serialize};

/// Why a [`crate::FrontendRequest::RecreateCoordinator`] was confirmed. Both paths
/// call the same engine-owned recreate, but the reason is recorded to the
/// audit log so an operator-initiated reset is distinguishable there from an
/// automatic model-mismatch replacement — and, separately, from a crash or
/// unexpected session loss, which is never audited under this event at all.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CoordinatorRecreateReason {
    /// The coordinator's running model no longer matches the engine's
    /// configured model; the operator confirmed replacing it. Also the
    /// default: it was the only caller before `reason` existed, so an
    /// older app talking to a newer engine (a `recreate_coordinator` line
    /// with no `reason` field) can only have meant this.
    #[default]
    ModelMismatch,
    /// The operator explicitly asked to reset the coordinator — to pick up
    /// a newer `claude` binary, clear accumulated context, or apply an
    /// environment change. Never triggered automatically.
    OperatorReset,
}
