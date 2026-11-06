//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    /// Capture metadata after start; the pre-pointer hook requires a durable head.
    pub async fn on_execution_started(&self, execution_id: &str) {
        if let Err(err) = self.snapshot_bound_pr(execution_id, true).await {
            tracing::warn!(execution_id, ?err, "execution_started snapshot failed");
        }
    }

    /// Record the bound PR's current head before the recovery pointer is created.
    ///
    /// Only the head lookup itself can fail the caller, because a transient
    /// GitHub failure is worth retrying. A bound PR that cannot be located
    /// locally (unparseable URL or remote, missing work item) fails every
    /// attempt identically, so it is logged and the pointer falls back to its
    /// trunk floor instead of failing the start.
    ///
    /// A deterministic head-lookup failure (the PR is gone, credentials are
    /// rejected) would fail every retry the same way, so it too is logged and
    /// the pointer falls back to its trunk floor immediately; only transport
    /// failures are returned for retry.
    ///
    /// The snapshot always belongs to this pre-start attempt, whether or not
    /// a pointer row already exists: a head recorded by an earlier attempt
    /// is cleared first, then replaced only when this attempt's own lookup
    /// succeeds. A skipped or failed lookup leaves no head, so the
    /// SHA-delta baseline never credits movement between attempts to this
    /// run. The recovery floor on an existing pointer row is left alone.
    pub async fn snapshot_pr_head_before(&self, execution_id: &str) -> Result<()> {
        self.snapshot_bound_pr(execution_id, false).await
    }

    /// The bound PR for an execution: its repo slug, number and URL.
    async fn locate_bound_pr(&self, execution: &boss_protocol::WorkExecution) -> Result<Option<(String, u64, String)>> {
        let bound_pr_url = match self.work_db.get_work_item(&execution.work_item_id)? {
            WorkItem::Task(task) | WorkItem::Chore(task) => {
                boss_engine_storage_values::branch_identity::task_bound_pr_url(&task)
                    .map(str::to_owned)
                    .or_else(|| {
                        if execution.kind == ExecutionKind::RevisionImplementation {
                            execution
                                .pr_url
                                .clone()
                                .filter(|u| !u.is_empty())
                                .or_else(|| self.work_db.get_revision_chain_root_pr_url(&task.id))
                        } else {
                            None
                        }
                    })
            }
            _ => None,
        };
        let Some(bound_pr_url) = bound_pr_url else {
            return Ok(None);
        };
        let repo_slug = parse_repo_slug(&execution.repo_remote_url)?;
        let pr_number =
            pr_number_from_url(&bound_pr_url).ok_or_else(|| anyhow::anyhow!("invalid bound PR URL: {bound_pr_url}"))?;
        Ok(Some((repo_slug, pr_number, bound_pr_url)))
    }

    async fn snapshot_bound_pr(&self, execution_id: &str, include_metadata: bool) -> Result<()> {
        let execution = self.work_db.get_execution(execution_id)?;
        if matches!(
            execution.kind,
            ExecutionKind::AutomationTriage | ExecutionKind::AnswerAgent | ExecutionKind::PrReviewGuide
        ) {
            tracing::debug!(execution_id, kind = %execution.kind.as_str(), "execution_started hook: kind never has a bound PR — skipping pr_head_before snapshot");
            return Ok(());
        }
        if !include_metadata && execution.pr_head_before.is_some() {
            self.work_db.clear_execution_pr_head_before(execution_id)?;
        }
        let located = match self.locate_bound_pr(&execution).await {
            Ok(located) => located,
            Err(err) if !include_metadata => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "pre-pointer snapshot: bound PR cannot be located; recording no pr_head_before"
                );
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        let Some((repo_slug, pr_number, bound_pr_url)) = located else {
            return Ok(());
        };
        // The pre-pointer call owns the snapshot for this pre-start attempt;
        // the running hook preserves it.
        if !include_metadata || execution.pr_head_before.is_none() {
            match self.branch_verifier.fetch_pr_head_oid(&repo_slug, pr_number).await {
                Ok(head_oid) => self.work_db.set_execution_pr_head_before(execution_id, &head_oid)?,
                Err(err) if !include_metadata && git_utils::gh_cli::is_deterministic(&err) => {
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "pre-pointer snapshot: head lookup cannot succeed on retry; recording no pr_head_before"
                    );
                    return Ok(());
                }
                Err(err) => return Err(err),
            }
        }
        if !include_metadata {
            return Ok(());
        }

        // Also snapshot the PR title and body as the baseline for the
        // metadata-only CI-fix finalize gate (issue #1252) and for `boss pr
        // body`'s read-modify-write. A CI-fix revision that repairs a
        // PR-description validator edits the body with no commit, so the
        // head SHA never moves; the body diff against this snapshot is the
        // only operator-visible evidence the worker contributed. Fetched
        // together in one `gh pr view` call since both come from the same
        // API response. Best-effort and independent of downstream
        // finalisation — an empty body is a valid snapshot, but a fetch
        // failure leaves both unset and the gate treats that as
        // inapplicable.
        match self
            .branch_verifier
            .fetch_pr_title_and_body(&repo_slug, pr_number)
            .await
        {
            Ok((title, body)) => {
                if let Err(err) = self.work_db.set_execution_pr_body_before(execution_id, &body) {
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "execution_started hook: failed to persist pr_body_before"
                    );
                } else {
                    tracing::debug!(
                        execution_id,
                        bound_pr_url = %bound_pr_url,
                        body_len = body.len(),
                        "execution_started hook: snapshotted pr_body_before for metadata-fix gate"
                    );
                }
                if let Err(err) = self.work_db.set_execution_pr_title_before(execution_id, &title) {
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "execution_started hook: failed to persist pr_title_before"
                    );
                } else {
                    tracing::debug!(
                        execution_id,
                        bound_pr_url = %bound_pr_url,
                        "execution_started hook: snapshotted pr_title_before"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    ?err,
                    "execution_started hook: fetch PR title/body failed; skipping snapshots"
                );
            }
        }
        Ok(())
    }

    /// Result of [`Self::try_retire_cleared_blocking_signal`]. `NotRetired`
    /// carries a [`ConflictSignalPrefetch`] whenever the method already
    /// probed the bound PR and found an active conflict attempt — so a
    /// subsequent call to [`Self::conflict_revision_stop_refusal`] can
    /// reuse that probe instead of re-deriving the parent attempt and
    /// re-querying GitHub for state this method just observed.
    fn blocking_signal_not_retired(prefetch: Option<Box<ConflictSignalPrefetch>>) -> BlockingSignalOutcome {
        BlockingSignalOutcome::NotRetired(prefetch)
    }

    /// Check whether the blocking signal for a conflict-resolution or
    /// CI-failure revision is already cleared even though the worker
    /// did not push any new commits (the `NoContribution` SHA-delta
    /// outcome). Returns [`BlockingSignalOutcome::Retired`] to
    /// short-circuit the nudge path on success;
    /// [`BlockingSignalOutcome::NotRetired`] falls through to the normal
    /// nudge, carrying along whatever probe context was already gathered.
    ///
    /// Probe result: if the merge probe is unavailable (e.g.
    /// [`NoopMergeProbe`] in tests, transient `gh` failure), the
    /// method returns `NotRetired(None)` so the nudge fires as before —
    /// safe fallback.
    ///
    /// Anti-re-entrancy: the attempt is marked `succeeded` before
    /// `finalize_pr_transition` runs, so a concurrent sweep cannot
    /// dispatch a new attempt for the same parent-chore signal.
    pub(super) async fn try_retire_cleared_blocking_signal(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
        bound_pr_url: &str,
    ) -> BlockingSignalOutcome {
        // Determine the parent chore ID that owns the conflict_resolutions /
        // ci_remediations attempt rows.
        //
        // Old-style kinds: `execution.work_item_id` IS the parent chore.
        // New-style `revision_implementation` (Phase 3+): `work_item_id` is
        // the revision task; the chore is its `parent_task_id`.
        let (parent_chore_id, product_id) = match execution.kind {
            ExecutionKind::ConflictResolution | ExecutionKind::CiRemediation => {
                // work_item_id is the chore directly.
                let product_id = match self.work_db.get_work_item(&execution.work_item_id) {
                    Ok(WorkItem::Task(t) | WorkItem::Chore(t)) => t.product_id.clone(),
                    _ => return Self::blocking_signal_not_retired(None),
                };
                (execution.work_item_id.clone(), product_id)
            }
            ExecutionKind::RevisionImplementation => {
                // work_item_id is the revision task. Only process it when
                // the revision was created by an engine-triggered conflict /
                // CI-fix attempt (`created_via` prefix).
                let task = match self.work_db.get_work_item(&execution.work_item_id) {
                    Ok(WorkItem::Task(t)) if t.kind == TaskKind::Revision => t,
                    _ => return Self::blocking_signal_not_retired(None),
                };
                let created_via = task.created_via.as_str();
                if !created_via.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX)
                    && !created_via.starts_with(CREATED_VIA_CI_FIX_PREFIX)
                {
                    return Self::blocking_signal_not_retired(None);
                }
                let Some(parent_id) = task.parent_task_id else {
                    return Self::blocking_signal_not_retired(None);
                };
                (parent_id, task.product_id.clone())
            }
            _ => return Self::blocking_signal_not_retired(None),
        };

        // Check for an active conflict-resolution or CI-remediation attempt.
        let conflict_attempt = self
            .work_db
            .active_conflict_resolution_for_work_item(&parent_chore_id)
            .unwrap_or(None);
        let ci_attempt = self
            .work_db
            .active_ci_remediation_for_work_item(&parent_chore_id)
            .unwrap_or(None);

        if conflict_attempt.is_none() && ci_attempt.is_none() {
            return Self::blocking_signal_not_retired(None);
        }

        // Probe the bound PR to check if the blocking signal is cleared.
        let probe = match self.merge_probe.probe(bound_pr_url).await {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url,
                    ?err,
                    "stop event: signal-cleared check: PR probe failed; \
                     falling through to nudge",
                );
                return Self::blocking_signal_not_retired(None);
            }
        };
        let open_status = match probe.state {
            PrLifecycleState::Open(ref s) => s.clone(),
            // PR merged or closed — not a signal-cleared case here.
            _ => return Self::blocking_signal_not_retired(None),
        };

        // --- Conflict signal check ---
        // Retire only when GitHub reports the PR *genuinely mergeable* again
        // (`Clean`) — NOT merely "not CONFLICTING". `Unknown` means the
        // mergeability recompute is still in flight (typically right after a
        // base move or this worker's own push). Retiring on `Unknown` records a
        // premature `succeeded` that can settle back to CONFLICTING; and when
        // the worker pushed nothing this run (the NoContribution caller), that
        // strands a `succeeded` attempt at an un-advanced head, whose
        // `(work_item, base, head)` UNIQUE key then permanently wedges
        // `conflict_watch`'s stale-base re-arm loop (mono#1398/#1764). Defer to
        // the next merge-poller probe instead of guessing.
        if conflict_attempt.is_some() && open_status.mergeability == OpenPrMergeability::Unknown {
            tracing::debug!(
                execution_id,
                bound_pr_url,
                "stop event: conflict signal-cleared check saw mergeable=UNKNOWN (GitHub recomputing); \
                 not retiring — deferring to the next merge-poller probe",
            );
        }
        if let Some(ref attempt) = conflict_attempt
            && open_status.mergeability == OpenPrMergeability::Clean
        {
            tracing::info!(
                execution_id,
                attempt_id = %attempt.id,
                bound_pr_url,
                "stop event: conflict already cleared — retiring attempt without nudging"
            );
            // Mark the attempt succeeded.
            match self.work_db.mark_conflict_resolution_succeeded(&attempt.id, None) {
                Ok(Some(succeeded)) => {
                    // Release old-style cube lease on the attempt (null for
                    // new Phase 3 revision-backed attempts).
                    if let Some(lease_id) = succeeded.cube_lease_id.as_deref()
                        && let Err(err) = self.cube_client.release_workspace(lease_id).await
                    {
                        tracing::debug!(
                            attempt_id = %attempt.id,
                            lease_id,
                            ?err,
                            "signal-cleared: conflict lease release failed \
                             (likely already released)",
                        );
                    }
                    self.publisher
                        .publish_frontend_event_on_product(
                            &product_id,
                            FrontendEvent::ConflictResolutionSucceeded {
                                attempt: self.work_db.remediation_entry(succeeded.list_entry()),
                                product_id: product_id.clone(),
                                work_item_id: parent_chore_id.clone(),
                                attempt_id: succeeded.id.clone(),
                                pr_url: bound_pr_url.to_owned(),
                            },
                        )
                        .await;
                }
                Ok(None) => {
                    tracing::debug!(
                        attempt_id = %attempt.id,
                        "signal-cleared: conflict attempt already terminal"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        attempt_id = %attempt.id,
                        ?err,
                        "signal-cleared: failed to mark conflict_resolution succeeded"
                    );
                }
            }
            // Snap parent chore back to in_review.
            match self.work_db.clear_chore_blocked_merge_conflict_for_attempt(
                &parent_chore_id,
                bound_pr_url,
                &attempt.id,
            ) {
                Ok(Some(_)) => {
                    self.publisher
                        .publish_work_item_changed(&product_id, &parent_chore_id, "merge_conflict_resolved")
                        .await;
                }
                Ok(None) => {
                    // WHERE guard missed — parent was already moved (e.g.
                    // by a concurrent sweep or human). Fine.
                }
                Err(err) => {
                    tracing::warn!(
                        work_item_id = %parent_chore_id,
                        ?err,
                        "signal-cleared: failed to clear blocked: merge_conflict"
                    );
                }
            }
            let outcome = self
                .finalize_pr_transition(
                    execution_id,
                    bound_pr_url.to_owned(),
                    WorkerPrCompletionTarget::InReview,
                    "stop_conflict_cleared",
                )
                .await;
            // Return the distinct outcome so tests and logs can identify this path.
            return BlockingSignalOutcome::Retired(match outcome {
                StopOutcome::PrDetected { pr_url } | StopOutcome::PrMerged { pr_url } => {
                    StopOutcome::SignalAlreadyCleared { pr_url }
                }
                other => other,
            });
        }

        // --- CI signal check ---
        if let Some(ref attempt) = ci_attempt
            && ci_attempt_signal_cleared(&attempt.failed_checks, &open_status.ci)
            && !boss_engine_storage_values::ci_contract::is_queue_side_failure_kind(attempt.failure_kind.as_deref())
        {
            tracing::info!(
                execution_id,
                attempt_id = %attempt.id,
                bound_pr_url,
                "stop event: CI already cleared — retiring attempt without nudging"
            );
            match self.work_db.mark_ci_remediation_succeeded(&attempt.id, None) {
                Ok(Some(succeeded)) => {
                    self.publisher
                        .publish_frontend_event_on_product(
                            &product_id,
                            FrontendEvent::CiRemediationSucceeded {
                                attempt: self.work_db.remediation_entry(succeeded.list_entry()),
                                product_id: product_id.clone(),
                                work_item_id: parent_chore_id.clone(),
                                attempt_id: attempt.id.clone(),
                                pr_url: bound_pr_url.to_owned(),
                            },
                        )
                        .await;
                }
                Ok(None) => {
                    tracing::debug!(
                        attempt_id = %attempt.id,
                        "signal-cleared: CI attempt already terminal"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        attempt_id = %attempt.id,
                        ?err,
                        "signal-cleared: failed to mark ci_remediation succeeded"
                    );
                }
            }
            match self
                .work_db
                .clear_chore_blocked_ci_failure(&parent_chore_id, bound_pr_url)
            {
                Ok(Some(_)) => {
                    self.publisher
                        .publish_work_item_changed(&product_id, &parent_chore_id, "ci_failure_resolved")
                        .await;
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        work_item_id = %parent_chore_id,
                        ?err,
                        "signal-cleared: failed to clear blocked: ci_failure"
                    );
                }
            }
            let outcome = self
                .finalize_pr_transition(
                    execution_id,
                    bound_pr_url.to_owned(),
                    WorkerPrCompletionTarget::InReview,
                    "stop_ci_cleared",
                )
                .await;
            return BlockingSignalOutcome::Retired(match outcome {
                StopOutcome::PrDetected { pr_url } | StopOutcome::PrMerged { pr_url } => {
                    StopOutcome::SignalAlreadyCleared { pr_url }
                }
                other => other,
            });
        }

        // Neither signal cleared this pass — hand the caller whatever probe
        // context we already gathered, so `conflict_revision_stop_refusal`
        // (the next thing every caller does) can reuse it instead of
        // re-deriving the parent attempt and re-probing GitHub for state
        // this method just observed.
        Self::blocking_signal_not_retired(Some(Box::new(ConflictSignalPrefetch { probe })))
    }
}
