use super::*;

impl ExecutionCoordinator {
    /// Validate after acquiring the lease, when no other lessee can replace the
    /// checkout. Missing identity (including legacy name-only labels) fails closed.
    ///
    /// `execution_id` is the replacement execution attempting this lease (not
    /// `prior`, the blocked predecessor it is trying to recover). It is used
    /// to consult the on-disk recovery marker as a second identity proof —
    /// see the comment below for why cube's own `last_task` label is not
    /// sufficient on a retry.
    pub(super) async fn verify_blocked_workspace(
        &self,
        execution_id: &str,
        prior: &WorkExecution,
        lease: &CubeWorkspaceLease,
        adapter: &Arc<dyn HostAdapter>,
    ) -> bool {
        if prior.preferred_workspace_id.as_deref() != Some(&lease.workspace_id) || lease.dirty_verified != Some(true) {
            return false;
        }
        // A prior dispatch attempt by this SAME replacement execution may
        // already have verified this exact workspace and recorded that via
        // the on-disk recovery marker (`RecoveryReport`, keyed by this
        // execution's own id — written by `reconcile_workspace_recovery`
        // right after a successful verified lease). That marker lives under
        // `.boss/` in the workspace itself and survives a later
        // deferral-release, unlike cube's own `last_task` label: releasing a
        // lease sets `last_task = COALESCE(task, last_task)`, and `task` was
        // stamped at lease time with THIS execution's own id (not
        // `prior.id`) — see `execution_task_summary`. So a deferral-release
        // between two dispatch attempts for the same execution silently
        // overwrites the `prior.id` marker the `last_task` check below
        // depends on, even though the workspace itself never changed. Trust
        // the on-disk marker first so that case does not force a spurious
        // fresh-workspace fallback.
        if boss_engine_recovery::recovery_apply::RecoveryReport::read_for(&lease.workspace_path, execution_id)
            .is_some_and(|report| report.from_execution_id == prior.id)
        {
            return true;
        }
        let status = match adapter.workspace_status(&lease.workspace_path).await {
            Ok(status) => status,
            Err(err) => {
                tracing::warn!(error = %err, "cannot verify blocked workspace ownership");
                return false;
            }
        };
        status.workspace_id == lease.workspace_id
            && status.lease_id.as_deref() == Some(&lease.lease_id)
            && status
                .last_task
                .as_deref()
                .and_then(|task| task.split_once(' '))
                .is_some_and(|(id, _)| id == prior.id)
    }
}
