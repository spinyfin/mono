//! Conflict revisions may finish only after GitHub verifies the resolution.
use super::*;

impl WorkerCompletionHandler {
    pub(super) async fn conflict_delivery_failure(
        &self,
        execution: &crate::work::WorkExecution,
        pr_url: &str,
    ) -> Option<StopOutcome> {
        if !self.is_merge_conflict_revision(execution) {
            return None;
        }
        let result = tokio::time::timeout(
            super::run_done_declaration::DECLARED_DELIVERY_NETWORK_BUDGET,
            conflict_stop_gate::verify_conflict_cleared(
                self.merge_probe.as_ref(),
                pr_url,
                self.conflict_unknown_backoff,
            ),
        )
        .await;
        let reason = match result {
            Ok(ConflictClearance::Cleared) => return None,
            Ok(ConflictClearance::StillConflicting {
                raw_mergeable,
                raw_merge_state_status,
            }) => format!("GitHub still reports {raw_mergeable} / {raw_merge_state_status}"),
            Ok(ConflictClearance::Indeterminate) => "GitHub mergeability remained UNKNOWN after retries".into(),
            Ok(ConflictClearance::Unavailable) => {
                "GitHub mergeability could not be verified (probe failed or PR is no longer open)".into()
            }
            Err(_) => "GitHub mergeability verification timed out".into(),
        };
        let detail = format!(
            "Conflict resolution was not delivered to {pr_url}: {reason}. A changed local or remote head alone does not prove the conflict is resolved."
        );
        if !self.finalize_worker_failure(execution, &detail).await {
            return Some(StopOutcome::DbError);
        }
        Some(StopOutcome::RunDoneDeclaredWithoutDelivery { detail })
    }
}
