//! Engine-host implementations of the services the merge poller's sweep requests
//! but does not own: conflict handling with its bounded background remediator,
//! design-doc pointer maintenance, and project population.
//!
//! The poller decides *when* a PR observation calls for each of them
//! ([`boss_engine_merge_poller::SweepHost`]); this module carries the decision
//! out against conflict watch and its escalation ladder, the design detector and
//! the Populator, all of which sit above the poller because they consume its
//! probe types. [`CoreSweepHost`] is the one place production and the engine's
//! integration tests assemble them.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use boss_engine_merge_poller::{
    ConflictCleared, ConflictHandling, ConflictResponse, LoopPasses, PopulationRequest, PopulationRequests, SweepPasses,
};
use boss_execution_contracts::{CubeClient, ExecutionPublisher};
use boss_observation_contracts::{DesignDetection, PrLifecycleProbe};

use crate::completion_adapters::CoreDocDetection;
use crate::conflict_remediation::{ConflictRemediationQueue, LadderRemediationJob};
use crate::conflict_watch::{self, ConflictRemediationMode};
use crate::metrics::Registry;
use crate::populator;
use crate::speculative_conflict::{self, SpeculativeCheckSchedule};
use crate::stacked_pr_structuring::{self, GhPrChangedFiles, StackingSchedule};
use crate::work::{PendingMergeCheck, PrStateChecker, WorkDb};

/// The sweep host a production merge-poller loop runs against.
pub struct CoreSweepHost {
    /// Conflict remediation runs HERE — on its own bounded tasks — not on the
    /// detection path. See [`crate::conflict_remediation`] for why: the escalation
    /// ladder leases a cube workspace and rebases, which awaited inline turned a
    /// 60-second sweep into a 32-minute detection blackout. `None` runs the
    /// ladder inline on the sweep's task when the sweep has a cube client.
    remediation: Option<ConflictRemediationQueue>,
    design: CoreDocDetection,
}

impl CoreSweepHost {
    /// The production host: conflict remediation is handed to a bounded
    /// background queue that drives the escalation ladder.
    pub fn new(work_db: Arc<WorkDb>, publisher: Arc<dyn ExecutionPublisher>, cube_client: Arc<dyn CubeClient>) -> Self {
        Self::with_remediation(Some(ConflictRemediationQueue::new(Arc::new(
            LadderRemediationJob::new(work_db, publisher, cube_client),
        ))))
    }

    /// A host with the given remediator — `None` runs the ladder inline when the
    /// sweep has a cube client (tests and pre-queue wiring), and a test can pass a
    /// queue over a job double to observe what the detection path hands it.
    pub fn with_remediation(remediation: Option<ConflictRemediationQueue>) -> Self {
        Self {
            remediation,
            design: CoreDocDetection,
        }
    }
}

#[async_trait]
impl ConflictHandling for CoreSweepHost {
    async fn on_conflict_detected(
        &self,
        work_db: &WorkDb,
        publisher: &dyn ExecutionPublisher,
        response: ConflictResponse<'_>,
        pr_checker: &dyn PrStateChecker,
        candidate: &PendingMergeCheck,
        probe: &PrLifecycleProbe,
    ) -> bool {
        let mode = match (response.ladder_enabled, self.remediation.as_ref(), response.cube_client) {
            (false, _, _) => ConflictRemediationMode::Disabled,
            (true, Some(queue), _) => ConflictRemediationMode::Deferred(queue),
            // No background remediator (tests, or a host built with
            // `with_remediation(None)`): run the ladder inline on the sweep task
            // when a cube client is supplied, rather than drop it.
            (true, None, Some(cube)) => ConflictRemediationMode::Inline(cube),
            (true, None, None) => ConflictRemediationMode::Disabled,
        };
        conflict_watch::on_conflict_detected_with(work_db, publisher, mode, pr_checker, candidate, probe).await
    }

    async fn on_conflict_resolved(
        &self,
        work_db: &WorkDb,
        publisher: &dyn ExecutionPublisher,
        cube_client: Option<&dyn CubeClient>,
        candidate: &PendingMergeCheck,
        cleared: ConflictCleared<'_>,
    ) -> bool {
        conflict_watch::on_resolved(
            work_db,
            publisher,
            cube_client,
            candidate,
            cleared.labels,
            cleared.raw_mergeable,
            cleared.raw_merge_state_status,
        )
        .await
    }

    fn retire_moot_conflict_revisions(
        &self,
        work_db: &WorkDb,
        candidate: &PendingMergeCheck,
        labels: &[String],
    ) -> usize {
        conflict_watch::retire_moot_conflict_revisions(work_db, candidate, labels)
    }
}

#[async_trait]
impl DesignDetection for CoreSweepHost {
    async fn on_task_doc_pr_detected(&self, work_db: &WorkDb, task_id: &str, product_id: &str, pr_url: &str) {
        self.design
            .on_task_doc_pr_detected(work_db, task_id, product_id, pr_url)
            .await;
    }

    async fn on_task_doc_pr_merged(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        product_id: &str,
        pr_url: &str,
        base_ref_name: Option<&str>,
    ) {
        self.design
            .on_task_doc_pr_merged(work_db, task_id, product_id, pr_url, base_ref_name)
            .await;
    }

    async fn on_design_pr_detected(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        product_id: &str,
        project_id: &str,
        pr_url: &str,
    ) {
        self.design
            .on_design_pr_detected(work_db, task_id, product_id, project_id, pr_url)
            .await;
    }

    async fn on_design_pr_merged(
        &self,
        work_db: &WorkDb,
        task_id: &str,
        product_id: &str,
        project_id: &str,
        pr_url: &str,
        base_ref_name: Option<&str>,
    ) {
        self.design
            .on_design_pr_merged(work_db, task_id, product_id, project_id, pr_url, base_ref_name)
            .await;
    }
}

impl PopulationRequests for CoreSweepHost {
    fn enqueue_from_merge(&self, work_db: &WorkDb, request: PopulationRequest) {
        populator::enqueue_from_merge(
            work_db,
            populator::PopulateContext {
                project_id: request.project_id,
                product_id: request.product_id,
                design_task_id: request.design_task_id,
                pr_url: request.pr_url,
            },
        );
    }
}

impl LoopPasses for CoreSweepHost {
    fn start_passes(&self) -> Box<dyn SweepPasses> {
        Box::new(CorePasses::default())
    }
}

/// The speculative conflict-prediction and stacked-PR structuring passes, each with
/// the throttle schedule it keeps across the loop's ticks.
#[derive(Default)]
struct CorePasses {
    speculative: SpeculativeCheckSchedule,
    stacking: StackingSchedule,
    decision_job: Option<tokio::task::JoinHandle<()>>,
}

#[async_trait]
impl SweepPasses for CorePasses {
    async fn run_conflict_decisions(&mut self, db: Arc<WorkDb>, publisher: Arc<dyn ExecutionPublisher>) {
        // A large batch of approved closes must not block PR detection. One
        // job at a time also prevents overlapping retries of the same action.
        if self.decision_job.as_ref().is_some_and(|job| !job.is_finished()) {
            return;
        }
        if let Some(job) = self.decision_job.take()
            && let Err(err) = job.await
        {
            tracing::error!(?err, "conflict decision delivery task failed");
        }
        self.decision_job = Some(boss_task_scope::spawn(async move {
            crate::conflict_inventory_actions::run(
                &db,
                publisher.as_ref(),
                &crate::conflict_inventory_actions::GithubActions,
                Arc::new(crate::merge_when_ready::CommandDirectMergeExecutor),
            )
            .await;
        }));
    }

    async fn run_speculative_pass(
        &mut self,
        work_db: &WorkDb,
        cube_client: &dyn CubeClient,
        metrics: &Registry,
        candidates: &[PendingMergeCheck],
    ) {
        speculative_conflict::run_speculative_pass(work_db, cube_client, metrics, &mut self.speculative, candidates)
            .await;
    }

    fn stacking_pass_due(&self, now: Instant) -> bool {
        self.stacking.pass_due(now)
    }

    async fn run_stacking_pass(
        &mut self,
        work_db: &WorkDb,
        publisher: &dyn ExecutionPublisher,
        metrics: &Registry,
        candidates: &[PendingMergeCheck],
    ) {
        stacked_pr_structuring::run_stacking_pass(
            work_db,
            publisher,
            &GhPrChangedFiles,
            metrics,
            &mut self.stacking,
            candidates,
        )
        .await;
    }
}
