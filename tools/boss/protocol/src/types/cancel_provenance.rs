//! Who ended an execution by decision, and why.
//!
//! Every path that moves an execution to a terminal-by-decision status
//! (`cancelled`, `abandoned`) must say, at the moment of the write, which
//! surface or engine subsystem did it ([`CancelSource`]), on whose behalf
//! (`actor`), and why (`reason`). The triple is stored on the execution row
//! (`work_executions.cancel_source` / `cancel_actor` / `cancel_reason`),
//! echoed in the engine audit event, and surfaced by `boss task show --json`,
//! `bossctl dispatch diagnose`, and the app.
//!
//! There is deliberately no default source and no placeholder reason: a
//! [`CancelProvenance`] can only be built through [`CancelProvenance::new`],
//! which rejects a blank actor or reason.

use serde::{Deserialize, Serialize};

/// The surface or engine path that ended an execution.
///
/// The first three variants are client surfaces: they arrive in a request
/// and the engine takes the caller's word for which one it is. Every other
/// variant names an engine subsystem and is only ever constructed inside the
/// engine, so a client cannot claim one (see [`Self::is_client_surface`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelSource {
    /// The Boss macOS app (a card gesture the app turned into a cancel).
    App,
    /// A `boss` CLI verb.
    BossCli,
    /// A `bossctl` verb (the coordinator's operator surface).
    Bossctl,
    /// `UpdateWorkItem` moved an `active` card back to Backlog (the kanban
    /// drag), tearing down its live worker.
    WorkItemMovedToBacklog,
    /// `DeleteWorkItem` tombstoned a task that still had a live worker.
    WorkItemDeleted,
    /// `UnpopulateProject` deleted a task that still had a live worker.
    ProjectUnpopulated,
    /// A `blocks` dependency edge landed on an actively-running dependent.
    DependencyBlock,
    /// The merge poller retired a revision whose parent PR merged.
    MergePollerParentMerged,
    /// The merge poller stopped a worker after its task reached Review.
    MergePollerReviewTransition,
    /// The dispatch scheduler preempted a lower-priority automation run.
    SchedulerPreemption,
    /// Dispatch admission refused a never-started execution.
    DispatchAdmission,
    /// The coordinator's start path failed before a worker came up.
    CoordinatorPreStart,
    /// A comment reclassification or a failed bind retired an answer agent.
    AnswerAgent,
    /// A newer PR review guide request replaced a running guide attempt.
    ReviewGuide,
    /// Applying a review verdict tombstoned a remediation it had minted.
    ReviewVerdict,
    /// The schema migration re-armed an item wedged by recovery failure.
    SchemaMigration,
    /// The double-spawn guard discarded a redundant duplicate execution.
    DoubleSpawnGuard,
    /// The review-batch machinery retired a member execution.
    ReviewBatch,
    /// `heal_ghost_active_chores` demoted an `active` card that never
    /// produced a run, abandoning its leftover executions.
    GhostActiveHeal,
    /// The stranded-execution sweep retired a row no dispatcher could start.
    StrandedExecutionSweep,
    /// Archiving or settling a work item dropped its not-yet-live executions.
    WorkItemSettled,
    /// Dispatch retired a merge-conflict revision whose PR was already resolved.
    StaleRevisionRetirement,
    /// `request_execution` abandoned a governing execution whose worker is gone.
    StaleGoverningExecution,
}

impl CancelSource {
    /// Every variant, in declaration order. Used by tests that must prove
    /// each source is both reachable and distinct.
    pub const ALL: &'static [CancelSource] = &[
        Self::App,
        Self::BossCli,
        Self::Bossctl,
        Self::WorkItemMovedToBacklog,
        Self::WorkItemDeleted,
        Self::ProjectUnpopulated,
        Self::DependencyBlock,
        Self::MergePollerParentMerged,
        Self::MergePollerReviewTransition,
        Self::SchedulerPreemption,
        Self::DispatchAdmission,
        Self::CoordinatorPreStart,
        Self::AnswerAgent,
        Self::ReviewGuide,
        Self::ReviewVerdict,
        Self::SchemaMigration,
        Self::DoubleSpawnGuard,
        Self::ReviewBatch,
        Self::GhostActiveHeal,
        Self::StrandedExecutionSweep,
        Self::WorkItemSettled,
        Self::StaleRevisionRetirement,
        Self::StaleGoverningExecution,
    ];

    /// The stable wire / database spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::BossCli => "boss_cli",
            Self::Bossctl => "bossctl",
            Self::WorkItemMovedToBacklog => "work_item_moved_to_backlog",
            Self::WorkItemDeleted => "work_item_deleted",
            Self::ProjectUnpopulated => "project_unpopulated",
            Self::DependencyBlock => "dependency_block",
            Self::MergePollerParentMerged => "merge_poller_parent_merged",
            Self::MergePollerReviewTransition => "merge_poller_review_transition",
            Self::SchedulerPreemption => "scheduler_preemption",
            Self::DispatchAdmission => "dispatch_admission",
            Self::CoordinatorPreStart => "coordinator_pre_start",
            Self::AnswerAgent => "answer_agent",
            Self::ReviewGuide => "review_guide",
            Self::ReviewVerdict => "review_verdict",
            Self::SchemaMigration => "schema_migration",
            Self::DoubleSpawnGuard => "double_spawn_guard",
            Self::ReviewBatch => "review_batch",
            Self::GhostActiveHeal => "ghost_active_heal",
            Self::StrandedExecutionSweep => "stranded_execution_sweep",
            Self::WorkItemSettled => "work_item_settled",
            Self::StaleRevisionRetirement => "stale_revision_retirement",
            Self::StaleGoverningExecution => "stale_governing_execution",
        }
    }

    /// `true` for the surfaces a client may name in a request. Engine
    /// subsystems are attributed by the engine itself and must never be
    /// accepted from the wire.
    pub fn is_client_surface(self) -> bool {
        matches!(self, Self::App | Self::BossCli | Self::Bossctl)
    }
}

impl std::fmt::Display for CancelSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A stored or supplied cancel source that is not in the vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownCancelSource(pub String);

impl std::fmt::Display for UnknownCancelSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown cancel source `{}`", self.0)
    }
}

impl std::error::Error for UnknownCancelSource {}

impl std::str::FromStr for CancelSource {
    type Err = UnknownCancelSource;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|source| source.as_str() == raw)
            .ok_or_else(|| UnknownCancelSource(raw.to_owned()))
    }
}

/// Why a [`CancelProvenance`] could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelProvenanceError {
    BlankActor,
    BlankReason,
}

impl std::fmt::Display for CancelProvenanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BlankActor => f.write_str("cancel provenance requires a non-blank actor"),
            Self::BlankReason => f.write_str("cancel provenance requires a non-blank reason"),
        }
    }
}

impl std::error::Error for CancelProvenanceError {}

/// The recorded answer to "who ended this execution, and why".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelProvenance {
    /// The surface or engine path that did it.
    pub source: CancelSource,
    /// The identity behind the source: a session, a peer, or the engine
    /// subsystem name when no finer identity exists.
    pub actor: String,
    /// A specific, human-readable cause.
    pub reason: String,
}

impl CancelProvenance {
    /// Build a provenance, trimming `actor` and `reason` and refusing blanks.
    /// There is no fallback: a caller with nothing to say must not cancel.
    pub fn new(
        source: CancelSource,
        actor: impl AsRef<str>,
        reason: impl AsRef<str>,
    ) -> Result<Self, CancelProvenanceError> {
        let actor = actor.as_ref().trim();
        let reason = reason.as_ref().trim();
        if actor.is_empty() {
            return Err(CancelProvenanceError::BlankActor);
        }
        if reason.is_empty() {
            return Err(CancelProvenanceError::BlankReason);
        }
        Ok(Self {
            source,
            actor: actor.to_owned(),
            reason: reason.to_owned(),
        })
    }

    /// Build a provenance for an engine-internal path, where the caller
    /// supplies compile-time-known text. A blank here is a programming error
    /// in the engine, so it panics loudly instead of recording nothing.
    pub fn engine(source: CancelSource, actor: &str, reason: impl AsRef<str>) -> Self {
        Self::new(source, actor, reason).unwrap_or_else(|err| panic!("engine cancel provenance for `{source}`: {err}"))
    }
}

impl std::fmt::Display for CancelProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} by {}: {}", self.source, self.actor, self.reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_spellings_round_trip_and_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for source in CancelSource::ALL {
            let raw = source.as_str();
            assert!(seen.insert(raw), "duplicate spelling {raw}");
            assert_eq!(raw.parse::<CancelSource>().unwrap(), *source);
            assert_eq!(serde_json::to_string(source).unwrap(), format!("\"{raw}\""));
        }
    }

    #[test]
    fn unknown_source_is_rejected() {
        assert!("explicit_cancel".parse::<CancelSource>().is_err());
        assert!(serde_json::from_str::<CancelSource>("\"unknown\"").is_err());
    }

    #[test]
    fn only_the_three_client_surfaces_are_client_surfaces() {
        let clients: Vec<_> = CancelSource::ALL
            .iter()
            .filter(|source| source.is_client_surface())
            .collect();
        assert_eq!(
            clients,
            vec![&CancelSource::App, &CancelSource::BossCli, &CancelSource::Bossctl]
        );
    }

    #[test]
    fn provenance_refuses_blank_actor_and_reason() {
        assert_eq!(
            CancelProvenance::new(CancelSource::Bossctl, "  ", "why"),
            Err(CancelProvenanceError::BlankActor)
        );
        assert_eq!(
            CancelProvenance::new(CancelSource::Bossctl, "me", "\n"),
            Err(CancelProvenanceError::BlankReason)
        );
        let ok = CancelProvenance::new(CancelSource::Bossctl, " me ", " because ").unwrap();
        assert_eq!((ok.actor.as_str(), ok.reason.as_str()), ("me", "because"));
    }

    #[test]
    #[should_panic(expected = "engine cancel provenance")]
    fn engine_constructor_panics_on_blank() {
        CancelProvenance::engine(CancelSource::DispatchAdmission, "dispatch_admission", " ");
    }
}
