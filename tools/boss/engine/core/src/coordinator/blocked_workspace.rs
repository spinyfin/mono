use super::*;

impl ExecutionCoordinator {
    /// Validate after acquiring the lease, when no other lessee can replace the
    /// checkout. Missing identity (including legacy name-only labels) fails closed.
    pub(super) async fn verify_blocked_workspace(
        &self,
        prior: &WorkExecution,
        lease: &CubeWorkspaceLease,
        adapter: &Arc<dyn HostAdapter>,
    ) -> bool {
        if prior.preferred_workspace_id.as_deref() != Some(&lease.workspace_id) || lease.dirty_verified != Some(true) {
            return false;
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
