//! Core-side binding of [`boss_execution_contracts::LifecycleServices`] to
//! the existing engine lifecycle modules. The coordinator talks only to the
//! trait; this adapter is the production implementation.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use boss_dispatch_events::DispatchEventSink;
use boss_execution_contracts::{
    ExecutionPublisher, HostAdapter, HostAdapterProvider, LeaseReleaseOutcome, LifecycleServices, LiveWorkerLiveness,
    PaneStatus, PidStatus, PreStartStage, ReattachSummary, RemoteLeaseReconcileOutcome, TeardownReason, TerminalCause,
    WorkerPaneReleaser,
};
use boss_protocol::WorkExecution;

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

/// Merge-parent deletion tripwire used by [`crate::coordinator::CommandCubeClient`].
pub async fn verify_deletion_tripwire(
    repo_slug: &str,
    head_before: &str,
    base_sha: &str,
    pr_number: u64,
) -> Vec<String> {
    let Some(head_after) = crate::merge_parent_deletion::fetch_pr_head_sha(repo_slug, pr_number).await else {
        return Vec::new();
    };
    crate::merge_parent_deletion::compute_merged_parent_deletions(repo_slug, head_before, base_sha, &head_after).await
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
        crate::answer_agent_observability::register_metrics(registry);
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
