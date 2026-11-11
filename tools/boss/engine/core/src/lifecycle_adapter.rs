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
    LifecycleServices, LiveWorkerLiveness, NoopExecutionPublisher, PaneStatus, PidStatus, ReattachSummary,
    RemoteLeaseReconcileOutcome, TeardownReason, TerminalCause, WorkerPaneReleaser, WorkerPool,
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
        let remote_occupancy;
        let occupancy: &dyn crate::workspace_lease_release::WorkspaceOccupancy = if adapter.host_id() == "local" {
            &crate::workspace_lease_release::HostOccupancy
        } else {
            remote_occupancy =
                crate::workspace_lease_release::OwningHostOccupancy(match execution.workspace_path.as_deref() {
                    Some(path) => adapter
                        .cleanup_execution_processes(&execution.id, std::path::Path::new(path))
                        .await
                        .unwrap_or_else(|err| {
                            tracing::error!(execution_id = %execution.id, host_id = adapter.host_id(), error = %err, "remote process cleanup failed before workspace release");
                            crate::workspace_lease_release::Occupancy::Unknown
                        }),
                    None => crate::workspace_lease_release::Occupancy::Unknown,
                });
            &remote_occupancy
        };
        crate::workspace_lease_release::apply_release_choosing(
            &self.work_db,
            execution,
            lease_id,
            crate::workspace_lease_release::ReleaseEvidence {
                cause,
                pane: PaneStatus::Gone,
                occupancy,
                preservation: &crate::workspace_lease_release::DbPreservation::new(&self.work_db, Some(adapter)),
            },
            move |preserved| async move {
                if preserved {
                    adapter.release_workspace_with_reset(lease_id).await
                } else {
                    adapter.release_workspace(lease_id).await
                }
            },
        )
        .await
    }

    async fn release_lease_after_proven_never_started(
        &self,
        execution: &WorkExecution,
        lease_id: &str,
        adapter: &dyn HostAdapter,
    ) -> LeaseReleaseOutcome {
        crate::workspace_lease_release::release_lease_after_proven_never_started(
            &self.work_db,
            execution,
            lease_id,
            adapter,
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
            Some(adapter),
            None,
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
        crate::answer_agent_observability::ensure_registered(registry);
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

    struct RecordingRelease(std::sync::Mutex<Vec<bool>>);
    crate::stub_host_adapter! { RecordingRelease {
        fn host_id(&self) -> &str { "local" }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            self.0.lock().unwrap().push(false);
            Ok(())
        }
        async fn release_workspace_with_reset(&self, _: &str) -> Result<()> {
            self.0.lock().unwrap().push(true);
            Ok(())
        }
    } }

    #[tokio::test]
    async fn proven_release_forces_reset_only_after_preservation_succeeds() {
        use crate::test_support::{create_test_chore, create_test_product};
        use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus};
        for preserved in [false, true] {
            let (dir, db) = open_db_arc();
            let product = create_test_product(&db);
            let chore = create_test_chore(&db, product.id, "Proven release");
            let execution = db
                .create_execution(
                    CreateExecutionInput::builder()
                        .work_item_id(chore.id)
                        .kind(ExecutionKind::ChoreImplementation)
                        .status(ExecutionStatus::Completed)
                        .cube_lease_id("lease")
                        .workspace_path(dir.path().join("workspace").to_string_lossy())
                        .build(),
                )
                .unwrap();
            if !preserved {
                // A positioned execution with an unavailable store cannot be preserved.
                db.record_execution_bookmark(&boss_engine_recovery::execution_bookmark::ExecutionBookmark {
                    execution_id: execution.id.clone(),
                    repo_path: "/tmp/unavailable".into(),
                    host_id: "local".into(),
                    base_commit: None,
                    pr_head_before: None,
                })
                .unwrap();
            }
            // The success mirror is a never-positioned run: no worker edits exist.
            let adapter = RecordingRelease(Default::default());
            let result = CoreLifecycleServices::new(db.clone())
                .apply_proven_workspace_release(&execution, "lease", TerminalCause::ProvenCompleted, &adapter)
                .await;
            assert_eq!(result, LeaseReleaseOutcome::Released);
            assert_eq!(*adapter.0.lock().unwrap(), vec![preserved]);
            assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_none());
        }
    }

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
