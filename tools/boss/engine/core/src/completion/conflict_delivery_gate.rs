//! Conflict revisions may finish only after GitHub verifies the resolution.
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::merge_poller::{MergeProbe, OpenPrMergeability, PrLifecycleProbe, PrLifecycleState};

/// What the conflict-delivery gate decided for a declared or Stop completion.
#[derive(Debug)]
pub(super) enum ConflictDeliveryDecision {
    /// Not a conflict revision, or GitHub reports the conflict cleared.
    /// Continue with the caller's existing target.
    Proceed,
    /// The PR merged or closed while the revision ran. Finalize through
    /// the existing merged/moot (`Done`) handling.
    Moot,
    /// Delivery failed and the execution has already been failed.
    Failed(StopOutcome),
    /// GitHub's answer does not yet describe the head the worker pushed (the
    /// read still shows an older head), so no verdict was reached and the
    /// execution was left live for the next Stop / `pr_recheck` to re-run the
    /// gate.
    Unsettled(StopOutcome),
}

/// Wraps a [`MergeProbe`] so a read whose head is not `expected_head` is
/// reported as `UNKNOWN` mergeability. GitHub can serve the previous head's
/// `CONFLICTING` for a moment after a push; the existing `UNKNOWN` retry loop
/// then waits it out instead of failing a resolution that did work.
struct HeadPinnedProbe<'a> {
    inner: &'a dyn MergeProbe,
    expected_head: &'a str,
    stale_seen: AtomicBool,
}

#[async_trait::async_trait]
impl MergeProbe for HeadPinnedProbe<'_> {
    async fn probe(&self, pr_url: &str) -> anyhow::Result<PrLifecycleProbe> {
        let mut probe = self.inner.probe(pr_url).await?;
        if let PrLifecycleState::Open(open) = &mut probe.state
            && probe
                .head_ref_oid
                .as_deref()
                .is_some_and(|head| head != self.expected_head)
        {
            open.mergeability = OpenPrMergeability::Unknown;
            self.stale_seen.store(true, Ordering::Relaxed);
        }
        Ok(probe)
    }
}

impl WorkerCompletionHandler {
    pub(super) async fn conflict_delivery_decision(
        &self,
        execution: &crate::work::WorkExecution,
        pr_url: Option<&str>,
    ) -> ConflictDeliveryDecision {
        if !self.is_merge_conflict_revision(execution) {
            return ConflictDeliveryDecision::Proceed;
        }
        let Some(pr_url) = pr_url.map(str::trim).filter(|url| !url.is_empty()) else {
            let detail =
                "Conflict resolution cannot be verified: this execution has no bound PR URL to check against GitHub."
                    .to_owned();
            if !self
                .finalize_worker_failure(execution, &detail, WorkerFailureBasis::DeliveredWithoutPr)
                .await
            {
                return ConflictDeliveryDecision::Failed(StopOutcome::DbError);
            }
            return ConflictDeliveryDecision::Failed(StopOutcome::ConflictDeliveryUnverified { detail });
        };
        // The head GitHub holds right now, read fresh. A mergeability read for
        // any other head predates the worker's push and proves nothing.
        let expected_head = self.fetch_pr_head_after(execution, pr_url).await;
        let pinned = expected_head.as_deref().map(|expected_head| HeadPinnedProbe {
            inner: self.merge_probe.as_ref(),
            expected_head,
            stale_seen: AtomicBool::new(false),
        });
        let probe: &dyn MergeProbe = match pinned.as_ref() {
            Some(pinned) => pinned,
            None => self.merge_probe.as_ref(),
        };
        let result = tokio::time::timeout(
            super::run_done_declaration::DECLARED_DELIVERY_NETWORK_BUDGET,
            conflict_stop_gate::verify_conflict_cleared(probe, pr_url, self.conflict_unknown_backoff),
        )
        .await;
        let stale_seen = pinned.as_ref().is_some_and(|p| p.stale_seen.load(Ordering::Relaxed));
        let reason = match result {
            Ok(ConflictClearance::Cleared) => return ConflictDeliveryDecision::Proceed,
            Ok(ConflictClearance::NotOpen) => return ConflictDeliveryDecision::Moot,
            Ok(ConflictClearance::StillConflicting {
                raw_mergeable,
                raw_merge_state_status,
            }) => format!("GitHub still reports {raw_mergeable} / {raw_merge_state_status}"),
            Ok(ConflictClearance::Indeterminate) if stale_seen => {
                tracing::info!(
                    execution_id = %execution.id,
                    pr_url,
                    expected_head = ?expected_head,
                    "conflict delivery gate: GitHub mergeability still describes an older head; leaving the run live for a recheck",
                );
                return ConflictDeliveryDecision::Unsettled(StopOutcome::DetectorFailed);
            }
            Ok(ConflictClearance::Indeterminate) => "GitHub mergeability remained UNKNOWN after retries".into(),
            Ok(ConflictClearance::Unavailable) => "GitHub mergeability could not be verified (probe failed)".into(),
            Err(_) => "GitHub mergeability verification timed out".into(),
        };
        let detail = format!(
            "Conflict resolution was not delivered to {pr_url}: {reason}. A changed local or remote head alone does not prove the conflict is resolved."
        );
        if !self
            .finalize_worker_failure(execution, &detail, WorkerFailureBasis::DeliveredWithoutPr)
            .await
        {
            return ConflictDeliveryDecision::Failed(StopOutcome::DbError);
        }
        ConflictDeliveryDecision::Failed(StopOutcome::ConflictDeliveryUnverified { detail })
    }
}
