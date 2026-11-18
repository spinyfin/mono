//! All revisions may finish only after GitHub verifies mergeability.
use super::*;

/// What the conflict-delivery gate decided for a declared or Stop completion.
#[derive(Debug)]
pub(super) enum ConflictDeliveryDecision {
    /// Not a revision, or GitHub reports the conflict cleared.
    /// Continue with the caller's existing target.
    Proceed,
    /// The PR merged while the revision ran.
    Merged,
    /// The PR closed without merging while the revision ran.
    ClosedUnmerged,
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
    expected_head: Option<&'a str>,
}

#[async_trait::async_trait]
impl MergeProbe for HeadPinnedProbe<'_> {
    async fn probe(&self, pr_url: &str) -> anyhow::Result<PrLifecycleProbe> {
        let mut probe = self.inner.probe(pr_url).await?;
        if let PrLifecycleState::Open(open) = &mut probe.state
            && (self.expected_head.is_none() || probe.head_ref_oid.as_deref() != self.expected_head)
        {
            open.mergeability = OpenPrMergeability::Unknown;
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
            return ConflictDeliveryDecision::Failed(self.delivery_unverified(execution, detail));
        };
        // A conflict revision's verdict must describe the head the worker
        // published: pin the probe to the attempt's remote branch head, read
        // via git rather than the eventually consistent GitHub PR record.
        let conflict_revision = self.is_merge_conflict_revision(execution);
        let attempt = if conflict_revision {
            let root = self.work_db.review_cycle_root_id(&execution.work_item_id);
            self.work_db
                .list_conflict_resolutions(None, &[], Some(&root), None)
                .ok()
                .and_then(|rows| {
                    rows.into_iter()
                        .find(|row| row.revision_task_id.as_deref() == Some(execution.work_item_id.as_str()))
                })
        } else {
            None
        };
        let result = tokio::time::timeout(super::run_done_declaration::DECLARED_DELIVERY_NETWORK_BUDGET, async {
            if !conflict_revision {
                return conflict_stop_gate::verify_conflict_cleared(
                    self.merge_probe.as_ref(),
                    pr_url,
                    self.conflict_unknown_backoff,
                )
                .await;
            }
            let expected_head = match attempt {
                Some(attempt) => self
                    .branch_verifier
                    .fetch_remote_head(&execution.repo_remote_url, &attempt.head_branch)
                    .await
                    .ok()
                    .filter(|head| !head.is_empty()),
                None => None,
            };
            let pinned = HeadPinnedProbe {
                inner: self.merge_probe.as_ref(),
                expected_head: expected_head.as_deref(),
            };
            conflict_stop_gate::verify_conflict_cleared(&pinned, pr_url, self.conflict_unknown_backoff).await
        })
        .await;
        // Reuse the completion circuit breaker with its own key so ordinary
        // worker nudges cannot spend the verification retries. Three rechecks,
        // at least a minute apart, let GitHub settle without an indefinite wait.
        let retry_key = format!("conflict-delivery:{}", execution.id);
        let reason = match &result {
            Ok(ConflictClearance::Cleared) => {
                self.nudge_breaker.forget(&retry_key);
                return ConflictDeliveryDecision::Proceed;
            }
            Ok(ConflictClearance::Merged) => {
                self.nudge_breaker.forget(&retry_key);
                return ConflictDeliveryDecision::Merged;
            }
            Ok(ConflictClearance::ClosedUnmerged) => {
                self.nudge_breaker.forget(&retry_key);
                return ConflictDeliveryDecision::ClosedUnmerged;
            }
            Ok(ConflictClearance::StillConflicting {
                raw_mergeable,
                raw_merge_state_status,
            }) => format!("GitHub still reports {raw_mergeable} / {raw_merge_state_status}"),
            Ok(ConflictClearance::Indeterminate) => "GitHub mergeability remained UNKNOWN after retries".into(),
            Ok(ConflictClearance::Unavailable) => "GitHub mergeability could not be verified (probe failed)".into(),
            Err(_) => "GitHub mergeability verification timed out".into(),
        };
        if conflict_revision && !matches!(result, Ok(ConflictClearance::StillConflicting { .. })) {
            let retry = !matches!(
                self.nudge_breaker.record(&retry_key, "unsettled", 3, (self.now_fn)(),),
                crate::nudge_breaker::NudgeDecision::Trip { .. }
            );
            if retry {
                tracing::info!(execution_id = %execution.id, %reason,
                    "conflict verification unsettled; retaining execution for bounded recheck");
                return ConflictDeliveryDecision::Unsettled(StopOutcome::DetectorFailed);
            }
        }
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
        self.nudge_breaker.forget(&retry_key);
        ConflictDeliveryDecision::Failed(self.delivery_unverified(execution, detail))
    }

    /// A conflict revision's refusal is reachable from every completion path
    /// the gate guards; an ordinary revision's stays the declared-run-done
    /// refusal.
    fn delivery_unverified(&self, execution: &crate::work::WorkExecution, detail: String) -> StopOutcome {
        if self.is_merge_conflict_revision(execution) {
            StopOutcome::ConflictDeliveryUnverified { detail }
        } else {
            StopOutcome::RunDoneDeclaredWithoutDelivery { detail }
        }
    }
}
