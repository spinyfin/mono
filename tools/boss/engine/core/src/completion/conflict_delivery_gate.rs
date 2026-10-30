//! All revisions may finish only after GitHub verifies mergeability.
use super::*;

/// What the conflict-delivery gate decided for a declared or Stop completion.
#[derive(Debug)]
pub(super) enum ConflictDeliveryDecision {
    /// Not a revision, or GitHub reports the conflict cleared.
    /// Continue with the caller's existing target.
    Proceed,
    /// The PR merged or closed while the revision ran. Finalize through
    /// the existing merged/moot (`Done`) handling.
    Moot,
    /// Delivery failed and the execution has already been failed.
    Failed(StopOutcome),
}

impl WorkerCompletionHandler {
    pub(super) async fn conflict_delivery_decision(
        &self,
        execution: &crate::work::WorkExecution,
        pr_url: Option<&str>,
    ) -> ConflictDeliveryDecision {
        if execution.kind != ExecutionKind::RevisionImplementation {
            return ConflictDeliveryDecision::Proceed;
        }
        let Some(pr_url) = pr_url.map(str::trim).filter(|url| !url.is_empty()) else {
            let detail =
                "Revision mergeability cannot be verified: this execution has no bound PR URL to check against GitHub."
                    .to_owned();
            if !self
                .finalize_worker_failure(execution, &detail, WorkerFailureBasis::DeliveredWithoutPr)
                .await
            {
                return ConflictDeliveryDecision::Failed(StopOutcome::DbError);
            }
            return ConflictDeliveryDecision::Failed(StopOutcome::RunDoneDeclaredWithoutDelivery { detail });
        };
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
            Ok(ConflictClearance::Cleared) => return ConflictDeliveryDecision::Proceed,
            Ok(ConflictClearance::NotOpen) => return ConflictDeliveryDecision::Moot,
            Ok(ConflictClearance::StillConflicting {
                raw_mergeable,
                raw_merge_state_status,
            }) => format!("GitHub still reports {raw_mergeable} / {raw_merge_state_status}"),
            Ok(ConflictClearance::Indeterminate) => "GitHub mergeability remained UNKNOWN after retries".into(),
            Ok(ConflictClearance::Unavailable) => "GitHub mergeability could not be verified (probe failed)".into(),
            Err(_) => "GitHub mergeability verification timed out".into(),
        };
        let files = self
            .work_db
            .revision_conflicting_files(&execution.work_item_id)
            .unwrap_or_default();
        let files = if files.is_empty() {
            "Conflicting files were not captured; inspect the rebased branch with jj resolve --list.".to_owned()
        } else {
            format!("Known conflicting files: {}.", files.join(", "))
        };
        let detail = format!(
            "A mergeable revision was not delivered to {pr_url}: {reason}. A changed local or remote head alone does not prove the conflict is resolved. {files}"
        );
        if !self
            .finalize_worker_failure(execution, &detail, WorkerFailureBasis::DeliveredWithoutPr)
            .await
        {
            return ConflictDeliveryDecision::Failed(StopOutcome::DbError);
        }
        ConflictDeliveryDecision::Failed(StopOutcome::RunDoneDeclaredWithoutDelivery { detail })
    }
}
