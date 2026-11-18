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
    LifecycleServices, LiveWorkerLiveness, NoopExecutionPublisher, PaneStatus, PidStatus, PredecessorRelease,
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

    async fn reconcile_absent_live_lease(
        &self,
        execution: &WorkExecution,
        lease_id: &str,
        adapter: Arc<dyn HostAdapter>,
        rows: Vec<boss_execution_contracts::CubeWorkspaceStatus>,
    ) -> anyhow::Result<LeaseReleaseOutcome> {
        use crate::workspace_lease_release::{HostOccupancy, OwningHostOccupancy, remote_cleanup_occupancy};
        if adapter.host_id() != "local" {
            let occupancy = remote_cleanup_occupancy(&self.work_db, execution, lease_id, adapter.as_ref()).await;
            return crate::workspace_lease_release::reconcile_absent_lease(
                &self.work_db,
                execution,
                lease_id,
                Some(adapter.as_ref()),
                None,
                Some(&OwningHostOccupancy(occupancy)),
                Some(&rows),
            )
            .await;
        }
        let db = Arc::clone(&self.work_db);
        let execution = execution.clone();
        let lease_id = lease_id.to_owned();
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                crate::workspace_lease_release::reconcile_absent_lease(
                    &db,
                    &execution,
                    &lease_id,
                    Some(adapter.as_ref()),
                    None,
                    Some(&HostOccupancy),
                    Some(&rows),
                )
                .await
            })
        })
        .await
        .unwrap_or_else(|err| Err(anyhow::anyhow!("workspace occupancy recheck failed: {err}")))
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
            remote_occupancy = crate::workspace_lease_release::OwningHostOccupancy(
                crate::workspace_lease_release::remote_cleanup_occupancy(&self.work_db, execution, lease_id, adapter)
                    .await,
            );
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

    async fn refresh_predecessor_lease(
        &self,
        execution: &WorkExecution,
        live: Option<&dyn LiveWorkerLiveness>,
        provider: &dyn HostAdapterProvider,
        local: Arc<dyn HostAdapter>,
        _at_bound: bool,
        release: PredecessorRelease,
    ) {
        use crate::workspace_lease_release::{
            LEASE_RECOVERY_UNAVAILABLE_ATTENTION_KIND, LEASE_UNRELEASED_ATTENTION_KIND, WorkspaceTipSource,
            refresh_attention, retry_pane_status, workspace_tip_source_from_listing,
        };
        let is_live = live.is_some_and(|live| live.snapshot().iter().any(|state| state.run_id == execution.id));
        if !execution.status.is_terminal() || is_live {
            return;
        }
        crate::workspace_lease_release::previous_occupancy_keep(&self.work_db, execution);
        let adapter = async {
            match self.work_db.latest_run_host_for_execution(&execution.id)?.as_deref() {
                None | Some("local") => Ok(local),
                Some(host_id) => {
                    boss_engine_completion::remote_host::resolve_host_adapter(&self.work_db, provider, host_id).await
                }
            }
        }
        .await;
        let adapter = match adapter {
            Ok(adapter) => adapter,
            Err(err) => {
                refresh_attention(
                    &self.work_db,
                    execution,
                    LEASE_RECOVERY_UNAVAILABLE_ATTENTION_KIND,
                    "Workspace recovery cannot reach its owning host",
                    &format!("Host resolution failed: {err:#}"),
                );
                return;
            }
        };
        let Some(lease_id) = execution.cube_lease_id.as_deref() else {
            return;
        };
        if retry_pane_status(&self.work_db, is_live, execution) != PaneStatus::Gone {
            return;
        }
        if adapter.host_id() != "local" {
            match self.work_db.latest_remote_run_for_execution(&execution.id) {
                Ok(Some(run)) => {
                    if let Some(pid) = run.remote_pid {
                        match adapter.probe_remote_worker_alive(pid).await {
                            Ok(Some(false)) => {}
                            result => {
                                refresh_attention(
                                    &self.work_db,
                                    execution,
                                    LEASE_RECOVERY_UNAVAILABLE_ATTENTION_KIND,
                                    "Workspace recovery cannot confirm its worker exited",
                                    &format!("Remote worker {pid} liveness probe did not confirm exit: {result:?}"),
                                );
                                return;
                            }
                        }
                    }
                }
                _ => return,
            }
        }
        let listing = adapter.list_workspaces().await;
        if listing.is_ok()
            && let Err(err) = self
                .work_db
                .resolve_attention_kind_for_execution(&execution.id, LEASE_RECOVERY_UNAVAILABLE_ATTENTION_KIND)
        {
            tracing::warn!(%err, "could not resolve workspace recovery availability");
        }
        match listing {
            Ok(rows) if !rows.iter().any(|row| row.lease_id.as_deref() == Some(lease_id)) => {
                let (tip_source, _) = workspace_tip_source_from_listing(
                    execution,
                    self.work_db.workspace_lease_boundary(&execution.id).ok().flatten(),
                    self.work_db.workspace_lease_generation(&execution.id).ok().flatten(),
                    &rows,
                );
                let outcome = if matches!(tip_source, WorkspaceTipSource::Live) {
                    self.reconcile_absent_live_lease(execution, lease_id, Arc::clone(&adapter), rows)
                        .await
                } else {
                    crate::workspace_lease_release::reconcile_absent_lease(
                        &self.work_db,
                        execution,
                        lease_id,
                        Some(adapter.as_ref()),
                        None,
                        None,
                        Some(&rows),
                    )
                    .await
                };
                if let Err(err) = outcome {
                    refresh_attention(
                        &self.work_db,
                        execution,
                        LEASE_UNRELEASED_ATTENTION_KIND,
                        "Workspace lease record could not be reconciled",
                        &format!("{err:#}"),
                    );
                }
                return;
            }
            Ok(_) => {}
            Err(err) => {
                refresh_attention(
                    &self.work_db,
                    execution,
                    LEASE_RECOVERY_UNAVAILABLE_ATTENTION_KIND,
                    "Workspace lease registry is unavailable",
                    &format!("Could not confirm cube lease {lease_id}: {err:#}"),
                );
                return;
            }
        }
        // The first release of a run belongs to whichever path ended it, so a
        // re-check normally only repeats a release that an earlier attempt
        // kept for occupancy. A caller that ended the run itself performs that
        // first release here, on the proof the leftover-lease sweep releases
        // on: a recorded worker shell that is confirmed dead. A run that never
        // recorded a shell stays with the sweeps.
        let owns_first_release = release == PredecessorRelease::Own
            && adapter.host_id() == "local"
            && crate::workspace_lease_release::recorded_shell_confirmed_dead(&self.work_db, &execution.id);
        if !owns_first_release && !crate::workspace_lease_release::previous_occupancy_keep(&self.work_db, execution) {
            return;
        }
        let db = Arc::clone(&self.work_db);
        let execution = execution.clone();
        let runtime = tokio::runtime::Handle::current();
        // Process snapshots and their retry sleeps must not block dispatch's executor.
        let result = tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                let Some(lease_id) = execution.cube_lease_id.as_deref() else {
                    return;
                };
                crate::workspace_lease_release::release_lease_after_inferred_death(
                    &db,
                    &execution,
                    PaneStatus::Gone,
                    Some(adapter.as_ref()),
                    None,
                    adapter.force_release_lease(lease_id, Some("recovery successor lease re-check")),
                )
                .await;
            })
        })
        .await;
        if let Err(err) = result {
            tracing::warn!(%err, "workspace occupancy recheck failed");
        }
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

    fn reevaluate_local_worker_holds(&self) {
        if let Err(err) = crate::local_worker_quarantine::reevaluate_local_worker_holds(&self.work_db) {
            tracing::warn!(
                error = %format!("{err:#}"),
                "could not re-evaluate the hold on unverified local workers; it stands until the next dispatch decision",
            );
        }
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

    fn spawn_failure_classification(&self, err: &anyhow::Error) -> Option<(&'static str, String)> {
        let failure = err.chain().find_map(|cause| cause.downcast_ref::<StartWorkerError>())?;
        Some((failure.class(), failure.cause_detail()))
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

    struct RemoteCleanup {
        cleanups: std::sync::Mutex<Vec<String>>,
        releases: std::sync::Mutex<u32>,
        fail: bool,
    }
    crate::stub_host_adapter! { RemoteCleanup {
        fn host_id(&self) -> &str { "remote" }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            *self.releases.lock().unwrap() += 1;
            Ok(())
        }
        async fn release_workspace_with_reset(&self, _: &str) -> Result<()> {
            *self.releases.lock().unwrap() += 1;
            Ok(())
        }
        async fn cleanup_execution_processes(&self, execution_id: &str, _: &std::path::Path) -> Result<crate::workspace_lease_release::Occupancy> {
            self.cleanups.lock().unwrap().push(execution_id.to_owned());
            if self.fail { anyhow::bail!("host unreachable"); }
            Ok(crate::workspace_lease_release::Occupancy::Empty)
        }
    } }

    #[tokio::test]
    async fn remote_release_stops_owned_processes_first_and_keeps_the_lease_when_that_fails() {
        use crate::test_support::{create_test_chore, create_test_product};
        use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus};
        for fail in [true, false] {
            let (dir, db) = open_db_arc();
            let product = create_test_product(&db);
            let chore = create_test_chore(&db, product.id, "Remote release");
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
            let adapter = RemoteCleanup {
                cleanups: Default::default(),
                releases: Default::default(),
                fail,
            };
            let result = CoreLifecycleServices::new(db.clone())
                .apply_proven_workspace_release(&execution, "lease", TerminalCause::ProvenCompleted, &adapter)
                .await;
            assert_eq!(
                adapter.cleanups.lock().unwrap().as_slice(),
                std::slice::from_ref(&execution.id)
            );
            if fail {
                assert!(matches!(result, LeaseReleaseOutcome::Kept { .. }), "{result:?}");
                assert_eq!(*adapter.releases.lock().unwrap(), 0);
                assert!(db.get_execution(&execution.id).unwrap().cube_lease_id.is_some());
            } else {
                assert_eq!(result, LeaseReleaseOutcome::Released);
                assert_eq!(*adapter.releases.lock().unwrap(), 1);
            }
        }
    }

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
