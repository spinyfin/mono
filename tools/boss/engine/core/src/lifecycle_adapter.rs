//! Core-side binding of [`boss_execution_contracts::LifecycleServices`] to
//! the existing engine lifecycle modules. The coordinator talks only to the
//! trait; this adapter is the production implementation.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use boss_dispatch_events::DispatchEventSink;
use boss_execution_contracts::{
    CubeClient, ExecutionPublisher, ExecutionRunner, HostAdapter, HostAdapterProvider, LeaseReleaseOutcome,
    LifecycleServices, LiveWorkerLiveness, NoopExecutionPublisher, PaneStatus, PidStatus, PreStartStage,
    ReattachSummary, RemoteLeaseReconcileOutcome, TeardownReason, TerminalCause, WorkerPaneReleaser, WorkerPool,
};
use boss_protocol::WorkExecution;

use crate::coordinator::ExecutionCoordinator;
use crate::host_adapter::LocalHostAdapter;
use crate::metrics::Registry;
use crate::spawn_flow::StartWorkerError;
use crate::work::WorkDb;

/// Production [`LifecycleServices`] implementation. Holds the work DB so
/// trait methods do not take it as an argument (work-db must not depend on
/// execution-contracts).
pub struct CoreLifecycleServices {
    work_db: Arc<WorkDb>,
}

impl CoreLifecycleServices {
    pub fn new(work_db: Arc<WorkDb>) -> Arc<Self> {
        Arc::new(Self { work_db })
    }
}

fn map_pid_status(status: crate::dead_pid_sweep::PidStatus) -> PidStatus {
    match status {
        crate::dead_pid_sweep::PidStatus::Alive => PidStatus::Alive,
        crate::dead_pid_sweep::PidStatus::Dead => PidStatus::Dead,
        crate::dead_pid_sweep::PidStatus::PermissionDenied => PidStatus::PermissionDenied,
        crate::dead_pid_sweep::PidStatus::Unknown(_) => PidStatus::Unknown,
    }
}

/// Merge-parent deletion tripwire, reached through
/// [`LifecycleServices::verify_deletion_tripwire`].
async fn verify_deletion_tripwire(repo_slug: &str, head_before: &str, base_sha: &str, pr_number: u64) -> Vec<String> {
    let Some(head_after) = crate::merge_parent_deletion::fetch_pr_head_sha(repo_slug, pr_number).await else {
        return Vec::new();
    };
    crate::merge_parent_deletion::compute_merged_parent_deletions(repo_slug, head_before, base_sha, &head_after).await
}

/// Test-support factories that install the core lifecycle implementation.
/// A local trait remains implementable when the coordinator moves crates.
/// Exposed for integration fixtures; production composition injects services explicitly.
#[doc(hidden)]
pub trait CoreCoordinatorConstructors: Sized {
    fn new(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
    ) -> Self;
    fn with_publisher(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
        publisher: Arc<dyn ExecutionPublisher>,
    ) -> Self;
}

/// Convenience coordinator constructors for test fixtures. They
/// live here, with the core adapter they install, so the coordinator's own
/// modules never name the concrete lifecycle implementation.
impl CoreCoordinatorConstructors for ExecutionCoordinator {
    /// Wraps `cube_client` and `execution_runner` in a `LocalHostAdapter`
    /// and calls [`Self::with_publisher`] with a no-op publisher.
    fn new(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
    ) -> Self {
        Self::with_publisher(
            work_db,
            worker_pool,
            cube_client,
            execution_runner,
            Arc::new(NoopExecutionPublisher),
        )
    }

    /// Wraps the cube/runner primitives in a `LocalHostAdapter` and installs
    /// [`CoreLifecycleServices`].
    fn with_publisher(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
        publisher: Arc<dyn ExecutionPublisher>,
    ) -> Self {
        let host_adapter = Arc::new(LocalHostAdapter::new(cube_client, execution_runner));
        let lifecycle = CoreLifecycleServices::new(Arc::clone(&work_db));
        Self::with_host_adapter_and_publisher(work_db, worker_pool, host_adapter, publisher, lifecycle)
    }
}

#[async_trait]
impl LifecycleServices for CoreLifecycleServices {
    async fn apply_proven_workspace_release(
        &self,
        execution: &WorkExecution,
        lease_id: &str,
        cause: TerminalCause,
        adapter: &dyn HostAdapter,
    ) -> LeaseReleaseOutcome {
        crate::workspace_lease_release::apply_release(
            &self.work_db,
            execution,
            lease_id,
            crate::workspace_lease_release::ReleaseEvidence {
                cause,
                pane: PaneStatus::Gone,
                occupancy: &crate::workspace_lease_release::EmptyOccupancy,
                bookmark: &crate::workspace_lease_release::DbBookmarkGate {
                    db: &self.work_db,
                    adapter: Some(adapter),
                },
            },
            adapter.release_workspace_with_reset(lease_id),
        )
        .await
    }

    async fn release_lease_after_proven_never_started(
        &self,
        execution: &WorkExecution,
        lease_id: &str,
        adapter: &dyn HostAdapter,
        stage: PreStartStage,
    ) -> LeaseReleaseOutcome {
        crate::workspace_lease_release::release_lease_after_proven_never_started(
            &self.work_db,
            execution,
            lease_id,
            adapter,
            stage,
        )
        .await
    }

    async fn release_refused_workspace(
        &self,
        execution: &WorkExecution,
        lease_id: &str,
        adapter: &dyn HostAdapter,
    ) -> LeaseReleaseOutcome {
        crate::workspace_lease_release::release_refused_workspace(&self.work_db, execution, lease_id, adapter).await
    }

    async fn release_lease_after_inferred_death(
        &self,
        execution: &WorkExecution,
        pane: PaneStatus,
        adapter: &dyn HostAdapter,
        reason: &str,
    ) -> LeaseReleaseOutcome {
        let Some(lease_id) = execution.cube_lease_id.as_deref() else {
            return LeaseReleaseOutcome::NoLease;
        };
        crate::workspace_lease_release::release_lease_after_inferred_death(
            &self.work_db,
            execution,
            pane,
            adapter.force_release_lease(lease_id, Some(reason)),
        )
        .await
    }

    async fn teardown_driver_workspace(
        &self,
        execution_id: &str,
        workspace_path: Option<&Path>,
        reason: TeardownReason,
    ) {
        crate::driver_teardown::teardown_driver_workspace(&self.work_db, execution_id, workspace_path, reason).await;
    }

    fn report_bookmark_failure(&self, execution: &WorkExecution, reason: &str) {
        crate::execution_bookmark_recovery::report_failure(&self.work_db, execution, reason);
    }

    fn block_unrecoverable_bookmark(&self, execution: &WorkExecution, err: &anyhow::Error) -> Result<bool> {
        crate::execution_bookmark_recovery::block_unrecoverable(&self.work_db, execution, err)
    }

    async fn reconcile_if_execution_dead(
        &self,
        execution: &WorkExecution,
        dispatch_events: &dyn DispatchEventSink,
    ) -> bool {
        crate::lost_workspace_sweep::reconcile_if_execution_dead(
            self.work_db.as_ref(),
            dispatch_events,
            execution,
            None,
        )
        .await
    }

    async fn reconcile_if_pane_dead(
        &self,
        execution: &WorkExecution,
        dispatch_events: &dyn DispatchEventSink,
        now_epoch_secs: i64,
        live_states: Option<&dyn LiveWorkerLiveness>,
    ) -> bool {
        crate::dead_pane_sweep::reconcile_if_pane_dead(
            self.work_db.as_ref(),
            dispatch_events,
            execution,
            now_epoch_secs,
            live_states,
            None,
        )
        .await
    }

    fn probe_pid(&self, pid: i32) -> PidStatus {
        map_pid_status(crate::dead_pid_sweep::probe_pid(pid))
    }

    async fn reattach_remote_runs(
        &self,
        provider: &dyn HostAdapterProvider,
        engine_events_socket: &str,
    ) -> ReattachSummary {
        crate::remote_reattach::reattach_remote_runs(&self.work_db, provider, engine_events_socket).await
    }

    async fn reconcile_remote_leases(
        &self,
        provider: &dyn HostAdapterProvider,
        dispatch_events: &dyn DispatchEventSink,
        pane_releaser: Option<&dyn WorkerPaneReleaser>,
    ) -> RemoteLeaseReconcileOutcome {
        crate::remote_lease_reconcile::reconcile_remote_leases(&self.work_db, provider, dispatch_events, pane_releaser)
            .await
    }

    fn register_answer_agent_metrics(&self, registry: &Registry) {
        use crate::answer_agent_observability::{ENQUEUED, FAILED, QUEUE_WAIT_MS, REPLIED, STARTED, SUPERSEDED};
        for handle in [&ENQUEUED, &STARTED, &REPLIED, &FAILED, &SUPERSEDED, &QUEUE_WAIT_MS] {
            if registry
                .counter_snapshot_one(handle.name())
                .is_none_or(|counter| counter.stale)
            {
                registry.register_counter(handle);
            }
        }
    }

    fn record_answer_agent_started(&self, registry: &Registry, queue_wait_ms: u64) {
        crate::answer_agent_observability::record_started(registry, queue_wait_ms);
    }

    fn record_conflict_class_counter(&self, registry: &Registry, product_id: &str, conflict_class: &str) {
        crate::merge_poller::record_conflict_class_counter(registry, product_id, conflict_class);
    }

    async fn notify_review_guide_changed(
        &self,
        publisher: &Arc<dyn ExecutionPublisher>,
        root_task_id: &str,
        reason: &str,
    ) {
        crate::execution_publisher::notify_review_guide_changed(&self.work_db, publisher, root_task_id, reason).await;
    }

    fn is_unconfirmed_spawn_abort(&self, err: &anyhow::Error) -> bool {
        err.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<StartWorkerError>(),
                Some(StartWorkerError::ViewerAbortFailed { .. })
            )
        })
    }

    async fn verify_deletion_tripwire(
        &self,
        repo_slug: &str,
        head_before: &str,
        base_sha: &str,
        pr_number: u64,
    ) -> Vec<String> {
        verify_deletion_tripwire(repo_slug, head_before, base_sha, pr_number).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_adapter::LocalHostAdapterProvider;
    use crate::test_support::{NoopCube, NoopRunner, open_db_arc};

    #[test]
    fn builder_registers_injected_lifecycle_metrics_in_shared_registry() {
        let (_dir, db) = open_db_arc();
        let lifecycle = CoreLifecycleServices::new(Arc::clone(&db));
        let metrics = Arc::new(Registry::new());
        crate::coordinator::register_metrics(&metrics);
        crate::dispatch_metrics::register_metrics(&metrics);
        assert_eq!(metrics.counter_value("answer_agent.started"), None);
        metrics.insert_stale_counter("answer_agent.started", "persisted counter", 4, 1);
        for expected in 5..=6 {
            let adapter: Arc<dyn HostAdapter> =
                Arc::new(LocalHostAdapter::new(Arc::new(NoopCube), Arc::new(NoopRunner)));
            let _coordinator = ExecutionCoordinator::builder()
                .work_db(Arc::clone(&db))
                .worker_pool(WorkerPool::new(1))
                .host_adapter(Arc::clone(&adapter))
                .host_adapter_provider(Arc::new(LocalHostAdapterProvider::new(adapter)))
                .lifecycle(lifecycle.clone())
                .metrics(Arc::clone(&metrics))
                .build();
            lifecycle.record_answer_agent_started(&metrics, 100);
            assert_eq!(metrics.counter_value("answer_agent.started"), Some(expected));
            assert_eq!(metrics.counter_value("answer_agent.queue_wait_ms"), Some(expected - 4));
            assert!(!metrics.counter_snapshot_one("answer_agent.started").unwrap().stale);
        }
    }
}
