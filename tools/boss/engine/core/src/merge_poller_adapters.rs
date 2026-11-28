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
use boss_feature_flags::{ConflictDeferralArm, FeatureFlagsStore};
use boss_observation_contracts::{DesignDetection, PrLifecycleProbe};

use crate::completion_adapters::CoreDocDetection;
use crate::conflict_deferral;
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
    /// Filled once the execution coordinator's host adapters exist; lets the
    /// remediation job probe a failed workspace on the host that refused it.
    host_adapters: Option<Arc<std::sync::OnceLock<Arc<dyn boss_execution_contracts::HostAdapterProvider>>>>,
    design: CoreDocDetection,
    merge_executor: Arc<dyn crate::merge_when_ready::DirectMergeExecutor>,
    /// Where the just-in-time conflict-resolution flags are read on every
    /// detection. `None` (tests, and any host built without flags) resolves
    /// every conflict on detection.
    feature_flags: Option<Arc<FeatureFlagsStore>>,
}

impl CoreSweepHost {
    /// The production host: conflict remediation is handed to a bounded
    /// background queue that drives the escalation ladder.
    pub fn new(work_db: Arc<WorkDb>, publisher: Arc<dyn ExecutionPublisher>, cube_client: Arc<dyn CubeClient>) -> Self {
        let job = LadderRemediationJob::new(work_db, publisher, cube_client);
        let slot = job.host_adapter_slot();
        let mut host = Self::with_remediation(Some(ConflictRemediationQueue::new(Arc::new(job))));
        host.host_adapters = Some(slot);
        host
    }

    /// Give conflict-workspace recovery the host adapters, so a remote host's
    /// refusal is probed on that host. No-op for hosts built without a job.
    pub fn set_host_adapter_provider(&self, provider: Arc<dyn boss_execution_contracts::HostAdapterProvider>) {
        if let Some(slot) = &self.host_adapters {
            let _ = slot.set(provider);
        }
    }

    /// A host with the given remediator — `None` runs the ladder inline when the
    /// sweep has a cube client (tests and pre-queue wiring), and a test can pass a
    /// queue over a job double to observe what the detection path hands it.
    pub fn with_remediation(remediation: Option<ConflictRemediationQueue>) -> Self {
        Self {
            remediation,
            host_adapters: None,
            design: CoreDocDetection,
            merge_executor: Arc::new(crate::merge_when_ready::CommandDirectMergeExecutor),
            feature_flags: None,
        }
    }

    /// Use `executor` for the merge continuation and for turning auto-merge
    /// off, so the poller and the merge handler share one executor.
    pub fn with_merge_executor(mut self, executor: Arc<dyn crate::merge_when_ready::DirectMergeExecutor>) -> Self {
        self.merge_executor = executor;
        self
    }

    /// Read the conflict-deferral flags from `feature_flags`, so a conflict on
    /// a PR in the deferring arm is recorded and resolved just in time instead
    /// of on detection (see [`crate::conflict_deferral`]).
    pub fn with_feature_flags(mut self, feature_flags: Arc<FeatureFlagsStore>) -> Self {
        self.feature_flags = Some(feature_flags);
        self
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
            (false, Some(queue), _) => ConflictRemediationMode::WorkerOnly(queue),
            (false, None, _) => ConflictRemediationMode::Disabled,
            (true, Some(queue), _) => ConflictRemediationMode::Deferred(queue),
            // No background remediator (tests, or a host built with
            // `with_remediation(None)`): run the ladder inline on the sweep task
            // when a cube client is supplied, rather than drop it.
            (true, None, Some(cube)) => ConflictRemediationMode::Inline(cube),
            (true, None, None) => ConflictRemediationMode::Disabled,
        };
        let deferral = self.feature_flags.as_deref().map_or(ConflictDeferralArm::Off, |flags| {
            conflict_deferral::arm_for(flags, candidate)
        });
        conflict_watch::on_conflict_detected_with(work_db, publisher, mode, pr_checker, candidate, probe, deferral)
            .await
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

    async fn resume_merge_after_conflict(
        &self,
        work_db: &WorkDb,
        publisher: &dyn ExecutionPublisher,
        candidate: &PendingMergeCheck,
    ) {
        crate::merge_when_ready::resume_after_conflict(work_db, publisher, candidate, self.merge_executor.as_ref())
            .await;
        // The same clean probe is when a merge request that outlived its
        // attempt can be settled. It runs second so a merge the line above
        // just re-requested is seen as the live submission it is.
        crate::merge_intent::settle_after_clean_probe(work_db, publisher, candidate).await;
    }

    async fn disarm_stopped_merge(
        &self,
        work_db: &WorkDb,
        publisher: &dyn ExecutionPublisher,
        candidate: &PendingMergeCheck,
    ) {
        crate::merge_when_ready::disarm_stopped_merge(work_db, publisher, candidate, self.merge_executor.as_ref())
            .await;
    }

    fn retire_moot_conflict_revisions(
        &self,
        work_db: &WorkDb,
        candidate: &PendingMergeCheck,
        labels: &[String],
    ) -> usize {
        // A definitive clean probe also ends a deferred conflict that went
        // away on its own. That is a fact about the PR, not upkeep, so it is
        // recorded whether or not the product has opted out of maintenance.
        usize::from(conflict_deferral::on_conflict_cleared(work_db, candidate))
            + conflict_watch::retire_moot_conflict_revisions(work_db, candidate, labels)
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

impl boss_engine_merge_poller::SweepHost for CoreSweepHost {
    fn slim_post_merge_review(&self, cycle_root_id: &str) -> bool {
        self.feature_flags.as_ref().is_some_and(|flags| {
            flags
                .resolve_experiment_arms(|key| boss_work_db::experiment_bucket(cycle_root_id, key))
                .is_treated(boss_feature_flags::POST_MERGE_REVIEW_SLIM_EXPERIMENT)
        })
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
}

#[async_trait]
impl SweepPasses for CorePasses {
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

#[cfg(test)]
mod slim_review_tests {
    use super::*;
    use boss_engine_merge_poller::SweepHost;
    use boss_feature_flags::{
        POST_MERGE_REVIEW_SLIM_AB_SPLIT_FLAG, POST_MERGE_REVIEW_SLIM_EXPERIMENT, TOKEN_EXPERIMENTS_FLAG,
    };

    #[test]
    fn host_uses_the_family_bucket_and_live_required_flags() {
        let dir = tempfile::tempdir().unwrap();
        let flags = Arc::new(FeatureFlagsStore::new(dir.path().join("flags.toml")));
        flags.load().unwrap();
        let host = CoreSweepHost::with_remediation(None).with_feature_flags(flags.clone());
        let key = POST_MERGE_REVIEW_SLIM_EXPERIMENT;
        assert!(!host.slim_post_merge_review("family"));
        flags.set(key, true).unwrap();
        assert!(!host.slim_post_merge_review("family"));
        flags.set("review_batch_fanout", true).unwrap();
        let mut seen = [false; 2];
        for n in 0..32 {
            let family = format!("family-{n}");
            let treated = boss_work_db::experiment_bucket(&family, key) < 50;
            seen[usize::from(treated)] = true;
            assert_eq!(host.slim_post_merge_review(&family), treated);
        }
        assert_eq!(seen, [true, true]);
        flags.set(POST_MERGE_REVIEW_SLIM_AB_SPLIT_FLAG, false).unwrap();
        assert!(host.slim_post_merge_review("family"));
        flags.set(TOKEN_EXPERIMENTS_FLAG, false).unwrap();
        assert!(!host.slim_post_merge_review("family"));
    }
}
