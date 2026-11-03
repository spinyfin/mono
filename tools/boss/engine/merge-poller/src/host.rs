//! Engine-host services the poller's sweep drives but does not own.
//!
//! The sweep decides *when* a PR observation calls for conflict handling,
//! design-doc pointer maintenance or project population; the engine carries
//! each out against the services that own the behaviour (conflict watch and
//! its escalation ladder, the design detector, the Populator). Those services
//! sit above the poller — they consume its probe types — so the poller sees
//! them only through the traits below, which the engine's composition root
//! implements and injects. Nothing here depends on the engine, the completion
//! service or the dispatch coordinator.
//!
//! Every method keeps the result and ordering of the call it replaced: the
//! sweep awaits the same futures at the same points and reads the same
//! boolean / count back.

use std::time::Instant;

use async_trait::async_trait;
use boss_execution_contracts::{CubeClient, ExecutionPublisher};
use boss_metrics::Registry;
use boss_observation_contracts::{DesignDetection, PrLifecycleProbe};
use boss_work_db::{PendingMergeCheck, PrStateChecker, WorkDb};

/// What the sweep knows about *how* a freshly observed conflict may be
/// remediated, before the host chooses between its remediators.
///
/// The sweep is the detection path: it must never await the escalation ladder
/// (a cube lease plus a real rebase) on its own task. Whether the ladder runs
/// at all, and whether it runs on the host's bounded background remediator or
/// inline, is the host's decision; these are only the inputs the sweep owns.
#[derive(Clone, Copy)]
pub struct ConflictResponse<'a> {
    /// The mechanical-rebase ladder is enabled (the `conflict_ladder_mechanical_rebase`
    /// flag as the completion service reports it). `false` takes the worker-only
    /// path: the conflict revision is spawned directly.
    pub ladder_enabled: bool,
    /// The cube client the sweep was given, for a host with no background
    /// remediator wired to fall back to running the ladder inline. `None` (and no
    /// remediator) leaves the ladder disabled.
    pub cube_client: Option<&'a dyn CubeClient>,
}

impl ConflictResponse<'_> {
    /// Detection that is not a fresh conflict (a recovery re-arm): never the ladder.
    pub const WORKER_ONLY: ConflictResponse<'static> = ConflictResponse {
        ladder_enabled: false,
        cube_client: None,
    };
}

/// The probe evidence that a recorded conflict has cleared.
#[derive(Clone, Copy)]
pub struct ConflictCleared<'a> {
    /// The PR's labels (an opt-out label gates the whole flow).
    pub labels: &'a [String],
    /// GitHub's raw `mergeable` value, quoted into the retire audit.
    pub raw_mergeable: &'a str,
    /// GitHub's raw `mergeStateStatus` value, quoted into the retire audit.
    pub raw_merge_state_status: &'a str,
}

/// Conflict detection and retirement, owned by the engine's conflict watch.
#[async_trait]
pub trait ConflictHandling: Send + Sync {
    /// A PR the sweep just probed is `CONFLICTING`. Records the attempt, publishes,
    /// and hands remediation to the host's remediator. Returns whether a conflict
    /// was flagged this call.
    async fn on_conflict_detected(
        &self,
        work_db: &WorkDb,
        publisher: &dyn ExecutionPublisher,
        response: ConflictResponse<'_>,
        pr_checker: &dyn PrStateChecker,
        candidate: &PendingMergeCheck,
        probe: &PrLifecycleProbe,
    ) -> bool;

    /// A PR with a recorded conflict probed definitively `MERGEABLE`. Retires the
    /// attempt and unblocks the parent. Returns whether anything was retired.
    async fn on_conflict_resolved(
        &self,
        work_db: &WorkDb,
        publisher: &dyn ExecutionPublisher,
        cube_client: Option<&dyn CubeClient>,
        candidate: &PendingMergeCheck,
        cleared: ConflictCleared<'_>,
    ) -> bool;

    /// Archives conflict-fix revisions that are moot now the PR is mergeable.
    /// Returns how many were closed.
    fn retire_moot_conflict_revisions(
        &self,
        work_db: &WorkDb,
        candidate: &PendingMergeCheck,
        labels: &[String],
    ) -> usize;
}

/// A merged design PR whose project the Populator should now fill with tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopulationRequest {
    pub project_id: String,
    pub product_id: String,
    pub design_task_id: String,
    pub pr_url: String,
}

/// Auto-population of a project's implementation tasks from a merged design doc.
pub trait PopulationRequests: Send + Sync {
    /// Enqueue the Populator for `request`. Cheap and synchronous: the planner call
    /// runs on a background task so the poller loop never blocks on it.
    fn enqueue_from_merge(&self, work_db: &WorkDb, request: PopulationRequest);
}

/// The optional passes that ride the loop's full-sweep cadence — speculative
/// conflict prediction and stacked-PR auto-structuring. Each keeps its own
/// throttle schedule, so the loop owns one runner for its lifetime.
#[async_trait]
pub trait SweepPasses: Send {
    /// Predict conflicts for `candidates` (the caller has checked the feature flag).
    async fn run_speculative_pass(
        &mut self,
        work_db: &WorkDb,
        cube_client: &dyn CubeClient,
        metrics: &Registry,
        candidates: &[PendingMergeCheck],
    );

    /// Whether the stacking pass's own throttle allows a pass at `now`. The loop
    /// asks before reading candidates, so throttled ticks cost no database read.
    fn stacking_pass_due(&self, now: Instant) -> bool;

    /// Offer stacked-PR restructuring for `candidates` (the caller has checked the
    /// feature flag and [`Self::stacking_pass_due`]).
    async fn run_stacking_pass(
        &mut self,
        work_db: &WorkDb,
        publisher: &dyn ExecutionPublisher,
        metrics: &Registry,
        candidates: &[PendingMergeCheck],
    );
}

/// Hands the loop its pass runner.
pub trait LoopPasses: Send + Sync {
    /// Fresh pass state for one loop. Called once, as the loop starts.
    fn start_passes(&self) -> Box<dyn SweepPasses>;
}

/// Everything the sweep needs from the engine host: conflict handling, design-doc
/// pointer maintenance (the same [`DesignDetection`] contract the completion
/// service consumes), project population and the loop's optional passes.
pub trait SweepHost: ConflictHandling + DesignDetection + PopulationRequests + LoopPasses {}

impl<T: ConflictHandling + DesignDetection + PopulationRequests + LoopPasses + ?Sized> SweepHost for T {}
