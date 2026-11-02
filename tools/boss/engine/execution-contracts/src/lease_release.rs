//! Lease-release evidence, decisions, and outcomes.

/// Why the run ended. Proven causes may release without an occupancy scan;
/// inferred death may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCause {
    /// Worker finished its turn and the completion path tore the pane down.
    ProvenCompleted,
    /// Pre-start failure: lease was acquired, no worker ever started.
    ProvenNeverStarted,
    /// Explicit stop; the pane and worker process have been confirmed torn down.
    ProvenStopped,
    /// Abandoned, orphaned, idle reap, or liveness timeout — death inferred.
    Inferred,
}

impl TerminalCause {
    pub fn label(self) -> &'static str {
        match self {
            Self::ProvenCompleted => "proven_completed",
            Self::ProvenNeverStarted => "proven_never_started",
            Self::ProvenStopped => "proven_stopped",
            Self::Inferred => "inferred",
        }
    }
}

/// Whether a live worker pane still maps to this execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStatus {
    Gone,
    Present,
}

/// Whether the recovery bookmark is confirmed written, when the run had work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookmarkStatus {
    NotRequired,
    Confirmed,
    Unconfirmed(String),
}

/// Why lease-release policy kept the lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    BookmarkUnconfirmed,
    PaneStillPresent,
    ProcessInWorkspace,
    OccupancyUnconfirmed,
}

impl KeepReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::BookmarkUnconfirmed => "bookmark_unconfirmed",
            Self::PaneStillPresent => "pane_still_present",
            Self::ProcessInWorkspace => "process_in_workspace",
            Self::OccupancyUnconfirmed => "occupancy_unconfirmed",
        }
    }
}

/// Policy result: release now, or keep the lease (cube TTL remains the backstop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseReleaseDecision {
    Release,
    Keep(KeepReason),
}

/// Outcome of attempting a cube release after policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseReleaseOutcome {
    Released,
    NoLease,
    Kept { reason: KeepReason },
    CubeFailed { error: String },
}

/// How far dispatch got before a pre-start failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreStartStage {
    /// The execution's recovery bookmark has not been written. The workspace
    /// holds only what cube placed there, so there is no worker work to lose.
    BeforeRecoveryBookmark,
    /// Bookmark creation was attempted or has run; its pointers must be
    /// confirmed before the workspace is returned.
    AtOrAfterRecoveryBookmark,
}
