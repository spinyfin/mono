//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    /// Hook invoked once when an execution transitions to `running`
    /// (from the coordinator's `start_execution_run` path). If the
    /// bound chore already has a PR URL (i.e. this is a resume /
    /// bounce-back of an already-bound chore), capture the PR's
    /// current head SHA into `work_executions.pr_head_before` so the
    /// Stop-boundary SHA-delta gate can verify the run's contribution.
    ///
    /// Best-effort: every step is fallible (work item lookup, repo
    /// slug parsing, PR number parsing, GitHub fetch) and on failure
    /// we log at WARN and leave `pr_head_before` unset. The gate
    /// treats a missing snapshot as "inapplicable" and falls through
    /// to the existing branch-keyed detector path — never noisier
    /// than the pre-change behaviour.
    pub async fn on_execution_started(&self, execution_id: &str) {
        let execution = match self.work_db.get_execution(execution_id) {
            Ok(execution) => execution,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    ?err,
                    "execution_started hook: unknown execution — skipping pr_head_before snapshot"
                );
                return;
            }
        };
        // `AutomationTriage`'s `work_item_id` is an automation id and
        // `AnswerAgent`'s is a comment id — neither is a product/project/task,
        // so `get_work_item` can never resolve them, and neither kind ever
        // has a bound PR to snapshot a head SHA for. Skip before paying for
        // (and warning on) a lookup that structurally cannot succeed.
        if matches!(
            execution.kind,
            ExecutionKind::AutomationTriage | ExecutionKind::AnswerAgent
        ) {
            tracing::debug!(
                execution_id,
                kind = %execution.kind.as_str(),
                "execution_started hook: kind never has a bound PR — skipping pr_head_before snapshot"
            );
            return;
        }
        let bound_pr_url = match self.work_db.get_work_item(&execution.work_item_id) {
            Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => {
                // For revision_implementation executions the task's own
                // pr_url is always NULL (design: revision tasks don't own
                // a PR).  Fall back to execution.pr_url (stamped at
                // dispatch), then to a chain-root lookup for executions
                // where execution.pr_url was not reliably set.
                crate::runner::task_bound_pr_url(&task).map(str::to_owned).or_else(|| {
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
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "execution_started hook: work item lookup failed; skipping pr_head_before snapshot"
                );
                return;
            }
        };
        let bound_pr_url = match bound_pr_url {
            Some(url) => url,
            None => {
                tracing::debug!(
                    execution_id,
                    "execution_started hook: chore has no bound pr_url — new-PR flow, no snapshot needed"
                );
                return;
            }
        };
        let repo_slug = match parse_repo_slug(&execution.repo_remote_url) {
            Ok(slug) => slug,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    repo_remote_url = %execution.repo_remote_url,
                    ?err,
                    "execution_started hook: cannot parse repo slug; skipping pr_head_before snapshot"
                );
                return;
            }
        };
        let pr_number = match pr_number_from_url(&bound_pr_url) {
            Some(n) => n,
            None => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    "execution_started hook: cannot parse PR number from bound URL; skipping pr_head_before snapshot"
                );
                return;
            }
        };
        let head_oid = match self.branch_verifier.fetch_pr_head_oid(&repo_slug, pr_number).await {
            Ok(oid) => oid,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    ?err,
                    "execution_started hook: fetch headRefOid failed; skipping pr_head_before snapshot"
                );
                return;
            }
        };
        if let Err(err) = self.work_db.set_execution_pr_head_before(execution_id, &head_oid) {
            tracing::warn!(
                execution_id,
                ?err,
                "execution_started hook: failed to persist pr_head_before"
            );
            return;
        }
        tracing::info!(
            execution_id,
            bound_pr_url = %bound_pr_url,
            head_oid = %head_oid,
            "execution_started hook: snapshotted pr_head_before for SHA-delta gate"
        );

        // Also snapshot the PR body as the baseline for the metadata-only
        // CI-fix finalize gate (issue #1252). A CI-fix revision that
        // repairs a PR-description validator edits the body with no commit,
        // so the head SHA never moves; the body diff against this snapshot
        // is the only operator-visible evidence the worker contributed.
        // Best-effort and independent of downstream finalisation — an empty
        // body is a valid snapshot, but a fetch failure leaves it unset and
        // the gate treats that as inapplicable.
        match self.branch_verifier.fetch_pr_body(&repo_slug, pr_number).await {
            Ok(body) => {
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
            }
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    ?err,
                    "execution_started hook: fetch PR body failed; skipping pr_body_before snapshot"
                );
            }
        }
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
            && !crate::ci_watch::is_queue_side_failure_kind(attempt.failure_kind.as_deref())
        {
            tracing::info!(
                execution_id,
                attempt_id = %attempt.id,
                bound_pr_url,
                "stop event: CI already cleared — retiring attempt without nudging"
            );
            match self.work_db.mark_ci_remediation_succeeded(&attempt.id, None) {
                Ok(Some(_)) => {
                    self.publisher
                        .publish_frontend_event_on_product(
                            &product_id,
                            FrontendEvent::CiRemediationSucceeded {
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
