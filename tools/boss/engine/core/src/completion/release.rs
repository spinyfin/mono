//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    /// Force-release the resources backing `execution_id`: tear down
    /// the libghostty pane and release the cube workspace. Idempotent —
    /// duplicate calls (e.g. completion-detection followed by a manual
    /// stop, or two clients racing to mark a chore done) become no-ops
    /// on the second pass via the registry's `take_slot_for_run`
    /// invariant and the DB's lease-id ownership transfer.
    ///
    /// Does NOT change the execution's status field. Callers that need
    /// the execution marked `completed` / `failed` should drive that
    /// transition through the appropriate `WorkDb` method.
    ///
    /// Returns what actually happened so callers that need cascade
    /// diagnosability (e.g. `cancel_and_release` tearing down a deleted
    /// work item's worker) can log a single line tying the outcome to
    /// the row that triggered it, rather than requiring a post-mortem
    /// to cross-reference this function's own log lines by execution id.
    pub async fn force_release(&self, execution_id: &str) -> ForceReleaseOutcome {
        // Pane release first. Idempotent on the registry side; the
        // implementation logs and skips when no slot is mapped.
        //
        // The outcome gates the cube release below: only a worker whose
        // pane was actually found and reaped frees its lease. A worker
        // still mid-spawn (no slot mapped yet, no pid to reap) reports
        // `NoLiveWorker` — releasing its lease now would hand a
        // workspace it is about to occupy back to cube, which re-leases
        // it into a same-workspace collision (T981). In that case the
        // lease stays held; the in-flight `run_execution` reaps the
        // worker once its spawn settles and releases the lease then.
        if matches!(
            self.pane_releaser.release_pane(execution_id).await,
            PaneReleaseOutcome::NoLiveWorker
        ) {
            tracing::info!(
                execution_id,
                "force_release: no live worker pane mapped (mid-spawn or already released); \
                 leaving the cube lease held — the in-flight run releases it after reaping, \
                 so an occupied workspace is never re-leased",
            );
            return ForceReleaseOutcome::HeldForInFlightSpawn;
        }

        // Cube release: claim ownership of the lease id atomically by
        // clearing it from the DB row before calling the cube CLI.
        // A concurrent caller will see `None` and skip.
        let lease_id = match self.work_db.clear_execution_workspace(execution_id) {
            Ok(Some(lease_id)) => lease_id,
            Ok(None) => return ForceReleaseOutcome::NoLeaseHeld,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "force_release: failed to clear execution workspace columns",
                );
                return ForceReleaseOutcome::WorkspaceColumnClearFailed;
            }
        };
        if let Err(err) = self.cube_client.release_workspace(&lease_id).await {
            tracing::warn!(
                execution_id,
                lease_id,
                ?err,
                "force_release: cube workspace release failed",
            );
            return ForceReleaseOutcome::LeaseReleaseFailed { lease_id };
        }
        ForceReleaseOutcome::Released { lease_id }
    }

    /// Stop a worker whose task was dragged back to Backlog by the user.
    /// Cancels the execution row in the DB (so the orphan sweep and
    /// reconciler won't re-dispatch it) then releases the pane and cube
    /// workspace via `force_release`. Does NOT demote the task status —
    /// the `UpdateWorkItem` handler already applied the user's `todo`
    /// patch before this is called.
    ///
    /// `work_item_id` is the row (task/chore) whose deletion or status
    /// change triggered this teardown. `reason` names what triggered the
    /// cancel (e.g. the kanban `active → todo` drag, or a row delete).
    /// Both are stamped on a single cascade trace line once the release
    /// completes so a post-mortem can grep `engine-trace.jsonl` for the
    /// row id and see the whole story — what triggered it, which
    /// execution it hit, and what the teardown actually did — without
    /// cross-referencing separate execution-id-only log lines. This
    /// closes the diagnosability gap behind the zombie-worker report
    /// where two deleted tasks kept running until `bossctl agents stop`
    /// was run by hand because nothing in the trace tied the row id to
    /// the executions that outlived it.
    pub async fn cancel_and_release(&self, work_item_id: &str, execution_id: &str, reason: &str) {
        let cancelled = match self.work_db.cancel_running_execution(execution_id) {
            Ok(cancelled) => cancelled,
            Err(err) => {
                tracing::warn!(
                    work_item_id,
                    execution_id,
                    reason,
                    ?err,
                    "cancel_and_release: failed to cancel execution; proceeding to release",
                );
                false
            }
        };
        let outcome = self.force_release(execution_id).await;
        tracing::info!(
            work_item_id,
            execution_id,
            reason,
            cancelled,
            outcome = outcome.label(),
            lease_id = match &outcome {
                ForceReleaseOutcome::Released { lease_id } | ForceReleaseOutcome::LeaseReleaseFailed { lease_id } =>
                    Some(lease_id.as_str()),
                _ => None,
            },
            "cancel_and_release: teardown cascade complete",
        );
    }

    /// Explicit human-initiated stop (`bossctl agents stop`). Unlike
    /// the normal `on_stop` hook path — which probes for a PR and
    /// waits for the worker to respond — this path is used when the
    /// operator wants the worker dead *now*. The differences:
    ///
    /// 1. Cancels the execution atomically in the DB (so the orphan
    ///    sweep and `reconcile_active_dispatch` don't re-dispatch the
    ///    work item the moment the pane is released and the worker
    ///    pool slot is freed).
    /// 2. Demotes the task from `active` → `todo` so the kanban
    ///    card moves back to the Backlog column instead of sitting
    ///    in Doing with no live worker.
    /// 3. Publishes a `work_item_changed` event so the UI and
    ///    downstream subscribers see the status transition.
    /// 4. Then calls `force_release` to kill the pane and free
    ///    the cube workspace.
    ///
    /// Idempotent: a second call for the same execution is a no-op
    /// at both the DB and the cube-release layers.
    pub async fn force_stop_execution(&self, execution_id: &str) {
        let (exec_cancelled, task_demoted) = match self.work_db.cancel_running_execution_and_demote_task(execution_id) {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "force_stop: failed to cancel execution / demote task — proceeding to release",
                );
                (false, false)
            }
        };

        if exec_cancelled || task_demoted {
            tracing::info!(
                execution_id,
                exec_cancelled,
                task_demoted,
                "force_stop: cancelled execution and demoted task",
            );
            // Publish work-item-changed so the UI refreshes. Requires
            // looking up the execution's work_item_id + product_id.
            if let Ok(execution) = self.work_db.get_execution(execution_id)
                && let Ok(work_item) = self.work_db.get_work_item(&execution.work_item_id)
            {
                let product_id = work_item.product_id().to_string();
                let wid = work_item_id(&work_item);
                self.publisher
                    .publish_work_item_changed(&product_id, &wid, "worker_force_stopped")
                    .await;
            }
        }

        self.force_release(execution_id).await;
    }

    /// Publish the more specific "stopped without a PR" signal so the
    /// frontend can paint a distinct activity icon (the live-state
    /// chore picks this up). Falls back to the same status string as
    /// `awaiting_input` because the execution row hasn't moved.
    pub(super) async fn publish_awaiting_pr(&self, execution: &crate::work::WorkExecution) {
        self.publisher
            .publish(
                &execution.id,
                &execution.work_item_id,
                execution.status.as_str(),
                "worker_awaiting_pr",
            )
            .await;
    }

    /// Resolve the PR already bound to this execution's chore, if any.
    /// Mirrors [`Self::evaluate_sha_delta_gate`]'s resolution: the
    /// chore's own structured `pr_url` is authoritative (it is set by
    /// whichever sibling execution opened the PR — for a
    /// `ci_remediation` exec that is the `chore_implementation` exec
    /// that shipped the original change). `revision_implementation`
    /// chores carry `pr_url = NULL` by design, so for that kind we fall
    /// back to `execution.pr_url` (the chain root's PR, stamped at
    /// dispatch).
    ///
    /// Used by the nudge path to decide whether a "produce a PR" nudge
    /// is even appropriate: when a PR is already bound the worker must
    /// be pointed at the existing branch, never told to `gh pr create`.
    pub(super) fn resolve_bound_pr_url(&self, execution: &crate::work::WorkExecution) -> Option<String> {
        match self.work_db.get_work_item(&execution.work_item_id) {
            Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => {
                crate::runner::task_bound_pr_url(&task).map(str::to_owned).or_else(|| {
                    if execution.kind == ExecutionKind::RevisionImplementation {
                        // Primary: execution.pr_url is stamped at dispatch time.
                        // Fallback: walk the parent chain to find the chain root's
                        // pr_url for executions where execution.pr_url was not set
                        // (e.g. older executions predating reliable dispatch stamping).
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
        }
    }

    /// True when `execution` is a `revision_implementation` execution whose
    /// task was spawned specifically to fix a merge conflict (`created_via`
    /// starts with [`CREATED_VIA_MERGE_CONFLICT_PREFIX`]). Used to relax the
    /// deliverable-satisfied gate: such a revision's job is done once the
    /// bound PR's mergeability clears — CI passing is a separate concern it
    /// was never asked to fix, and gating completion on it as well produces
    /// the "already resolved but still nudged" loop this check exists to
    /// close.
    pub(super) fn is_merge_conflict_revision(&self, execution: &crate::work::WorkExecution) -> bool {
        if execution.kind != ExecutionKind::RevisionImplementation {
            return false;
        }
        matches!(
            self.work_db.get_work_item(&execution.work_item_id),
            Ok(WorkItem::Task(ref task))
                if task.kind == TaskKind::Revision
                    && task.created_via.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX)
        )
    }

    /// True when `execution` is a merge-conflict revision whose parent
    /// chore still owns a live (`pending` / `running`) `conflict_resolutions`
    /// row — i.e. the engine's own ledger still believes this conflict needs
    /// resolving.
    ///
    /// Requiring a live attempt is what keeps [`Self::conflict_revision_stop_refusal`]
    /// from second-guessing a worker that correctly escalated: a worker that
    /// ran `boss engine conflicts mark-failed … --reason product_decision_required`
    /// leaves the attempt terminal, and dogging it about GitHub's `mergeable`
    /// after it deliberately declined to resolve would be exactly the
    /// pointless nudging this change exists to reduce.
    ///
    /// Also checks that the live attempt is the one THIS revision was
    /// spawned for: `created_via` carries the owning attempt id as
    /// `merge-conflict:<crz_id>`. Without this check, a revision whose own
    /// attempt was retired (superseded by a fresh attempt minted for a
    /// later base move) would be refused and nudged about a conflict a
    /// different, newer revision now owns. Falls back to "any live
    /// attempt on the parent" when `created_via` doesn't carry a
    /// parseable id (older executions).
    pub(super) fn has_active_conflict_attempt(&self, execution: &crate::work::WorkExecution) -> bool {
        let Ok(WorkItem::Task(task)) = self.work_db.get_work_item(&execution.work_item_id) else {
            return false;
        };
        let Some(parent_id) = task.parent_task_id else {
            return false;
        };
        let Some(attempt) = self
            .work_db
            .active_conflict_resolution_for_work_item(&parent_id)
            .unwrap_or(None)
        else {
            return false;
        };
        match task.created_via.strip_prefix(CREATED_VIA_MERGE_CONFLICT_PREFIX) {
            Some(owning_attempt_id) if !owning_attempt_id.is_empty() => attempt.id == owning_attempt_id,
            _ => true,
        }
    }

    /// GitHub-authoritative refusal gate for a merge-conflict revision that
    /// reached a Stop boundary without moving the bound PR's head.
    ///
    /// The generic [`probe_push_to_existing_pr`] nudge ends with *"if there
    /// is nothing left to do, say so"*. For a conflict revision that is the
    /// wrong contract — whether a conflict remains is objectively checkable
    /// and the engine already holds the bound PR URL, so it must check
    /// rather than take the worker's word. (Incident 2026-07-23,
    /// spinyfin/mono#2070: the worker declared the conflict "already
    /// resolved and pushed in a prior attempt" off divergent local `jj`
    /// state, having never queried `mergeable` at all; GitHub said
    /// `CONFLICTING` / `DIRTY` throughout, and the run only recovered after
    /// a manual message.)
    ///
    /// Returns `Some((probe_text, fingerprint))` when the engine must refuse
    /// the "nothing left to do" reading, or `None` when the claim is
    /// corroborated (GitHub reports the PR mergeable), the gate does not
    /// apply, or GitHub could not be reached — in which case the caller
    /// falls through to its ordinary handling.
    ///
    /// `prefetched` is the probe context [`Self::try_retire_cleared_blocking_signal`]
    /// already gathered immediately before this call, when the caller
    /// routes through it first (both `on_stop` call sites do). Reusing it
    /// avoids re-deriving the parent attempt and re-probing a PR that
    /// method just probed, and — because that method only reaches its
    /// fall-through path when mergeability was NOT `Clean` — this call
    /// spends its own probe budget only on the `UNKNOWN` retries it
    /// genuinely adds. Pass `None` when no such prior probe exists (e.g.
    /// the inconclusive-SHA-delta call site, which does not call
    /// `try_retire_cleared_blocking_signal` first); this method then falls
    /// back to deriving everything itself.
    pub(super) async fn conflict_revision_stop_refusal(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
        bound_pr_url: &str,
        prefetched: Option<Box<ConflictSignalPrefetch>>,
    ) -> Option<(String, String)> {
        if !self.is_merge_conflict_revision(execution) {
            return None;
        }
        // Always recompute ownership via `has_active_conflict_attempt` rather
        // than trusting `prefetched.conflict_attempt_active` — that flag
        // comes from `try_retire_cleared_blocking_signal`'s looser "any live
        // attempt on the parent" check (it has no crz_id to match against at
        // that call site), so reusing it here would silently skip the
        // crz_id ownership match on both `NoContribution` call sites, the
        // exact path the 2026-07-23 incident took. The PR probe is still
        // reused from `prefetched` below; only the has-attempt boolean is
        // excluded from reuse.
        if !self.has_active_conflict_attempt(execution) {
            return None;
        }
        let clearance = match prefetched {
            Some(ctx) => {
                conflict_stop_gate::verify_conflict_cleared_from(
                    self.merge_probe.as_ref(),
                    bound_pr_url,
                    self.conflict_unknown_backoff,
                    Some(ctx.probe),
                )
                .await
            }
            None => {
                conflict_stop_gate::verify_conflict_cleared(
                    self.merge_probe.as_ref(),
                    bound_pr_url,
                    self.conflict_unknown_backoff,
                )
                .await
            }
        };
        match clearance {
            ConflictClearance::StillConflicting {
                raw_mergeable,
                raw_merge_state_status,
            } => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url,
                    %raw_mergeable,
                    %raw_merge_state_status,
                    "stop event: merge-conflict revision pushed nothing but GitHub still reports the \
                     PR conflicting — refusing the 'already resolved' reading and probing with the \
                     live values",
                );
                Some((
                    conflict_stop_gate::probe_conflict_still_present(
                        bound_pr_url,
                        &raw_mergeable,
                        &raw_merge_state_status,
                    ),
                    format!("conflict_unresolved:{bound_pr_url}:{raw_mergeable}"),
                ))
            }
            ConflictClearance::Indeterminate => {
                tracing::info!(
                    execution_id,
                    bound_pr_url,
                    "stop event: merge-conflict revision pushed nothing and GitHub's mergeability is \
                     still UNKNOWN — refusing to read that as resolved",
                );
                Some((
                    conflict_stop_gate::probe_conflict_mergeability_unknown(bound_pr_url),
                    format!("conflict_mergeable_unknown:{bound_pr_url}"),
                ))
            }
            ConflictClearance::Cleared | ConflictClearance::Unavailable => None,
        }
    }
}
