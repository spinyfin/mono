//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    /// Catch-all finaliser for conflict-resolution workers — the
    /// merge-conflict twin of [`Self::finalize_ci_remediation_attempt`].
    /// Fires for every Stop event on a conflict-resolution revision
    /// (`revision_implementation` whose `created_via` is `merge-conflict:*`)
    /// or a legacy `conflict_resolution` execution.
    ///
    /// **Why this exists.** Conflict resolution was unified onto the
    /// revision substrate (`unify-pr-remediation-on-revisions.md`); the
    /// fix vehicle is now a `revision_implementation` execution. When such
    /// a worker stops WITHOUT pushing — it stalled before the push step,
    /// hit a stop condition without classifying, or was nudged to
    /// exhaustion — nothing retired the bound `conflict_resolutions`
    /// ledger row. It stranded `pending` forever: the operator sees a
    /// "revision task that does nothing", and once `main` moves again the
    /// detector mints a fresh conflict revision against the new base SHA
    /// (the re-mint loop). This finaliser closes that gap: a worker that
    /// the auto-nudge breaker has parked (no push, no resume coming) marks
    /// the attempt `failed` with [`CONFLICT_NO_PUSH_REASON`], so the
    /// parent surfaces `blocked: merge_conflict` for human attention
    /// instead of looping, and the ledger is honest.
    ///
    /// **Conservatism.** Unlike the CI twin (which marks failed on the
    /// first idle Stop), this fires ONLY on
    /// [`StopOutcome::NudgeBreakerParked`] — the genuine "no push, and the
    /// engine has stopped trying" terminal. While the worker is still
    /// being nudged (`AwaitingInput`) the attempt is left `pending` so a
    /// worker that resumes and pushes is never prematurely failed and the
    /// detector's in-flight dedup keeps holding (no duplicate revision).
    ///
    /// Idempotent — [`WorkDb::mark_conflict_resolution_failed`] WHERE-guards
    /// on `status IN ('pending', 'running')`, so a duplicate call after a
    /// terminal transition writes nothing.
    pub async fn finalize_conflict_resolution_attempt(
        &self,
        execution: &crate::work::WorkExecution,
        outcome: &StopOutcome,
    ) {
        // Only the genuine "stopped, no push, breaker gave up" terminal
        // retires the attempt. Bail early on every other outcome so we
        // never touch the ledger for a worker that pushed, is still being
        // nudged, or hit a transient probe failure.
        if !matches!(outcome, StopOutcome::NudgeBreakerParked { .. }) {
            return;
        }

        // Resolve the parent chore that owns the `conflict_resolutions`
        // row. Mirrors `try_retire_cleared_blocking_signal`: for a
        // revision the chore is `parent_task_id`, and we only act when the
        // revision is merge-conflict provenance (a `ci-fix:` revision has
        // no conflict attempt to retire).
        let parent_chore_id = match execution.kind {
            ExecutionKind::ConflictResolution => execution.work_item_id.clone(),
            ExecutionKind::RevisionImplementation => {
                let task = match self.work_db.get_work_item(&execution.work_item_id) {
                    Ok(WorkItem::Task(t)) if t.kind == TaskKind::Revision => t,
                    _ => return,
                };
                if !task.created_via.as_str().starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX) {
                    return;
                }
                match task.parent_task_id {
                    Some(parent) => parent,
                    None => return,
                }
            }
            _ => return,
        };

        let attempt = match self.work_db.active_conflict_resolution_for_work_item(&parent_chore_id) {
            Ok(Some(attempt)) => attempt,
            Ok(None) => {
                tracing::debug!(
                    execution_id = %execution.id,
                    work_item_id = %parent_chore_id,
                    "conflict-resolution finalizer: no active attempt; nothing to do",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %parent_chore_id,
                    ?err,
                    "conflict-resolution finalizer: failed to look up active attempt",
                );
                return;
            }
        };
        // Already past the "live attempt with no recorded outcome" window
        // — a push retired it `succeeded`, the worker classified it via
        // `mark-failed`, or some other path closed the row. Note that
        // revision-backed attempts stay `pending` in production
        // (`mark_conflict_resolution_running` is only used by the legacy
        // bespoke dispatch), so `pending` is a live state here, not a
        // no-op.
        if !matches!(attempt.status.as_str(), "pending" | "running")
            || attempt.head_sha_after.is_some()
            || attempt.failure_reason.is_some()
        {
            return;
        }

        let updated = match self
            .work_db
            .mark_conflict_resolution_failed(&attempt.id, CONFLICT_NO_PUSH_REASON)
        {
            Ok(Some(row)) => row,
            Ok(None) => {
                tracing::debug!(
                    attempt_id = %attempt.id,
                    "conflict-resolution finalizer: attempt already terminal between probes",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?err,
                    "conflict-resolution finalizer: failed to mark attempt failed",
                );
                return;
            }
        };

        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %parent_chore_id,
            attempt_id = %updated.id,
            pr_url = %updated.pr_url,
            reason = CONFLICT_NO_PUSH_REASON,
            ?outcome,
            "conflict-resolution finalizer: worker parked without pushing; attempt marked failed",
        );

        self.publisher
            .publish_frontend_event_on_product(
                &updated.product_id,
                FrontendEvent::ConflictResolutionFailed {
                    product_id: updated.product_id.clone(),
                    work_item_id: updated.work_item_id.clone(),
                    attempt_id: updated.id.clone(),
                    pr_url: updated.pr_url.clone(),
                    failure_reason: CONFLICT_NO_PUSH_REASON.to_owned(),
                },
            )
            .await;
    }

    /// Phase 10 #33: catch-all finaliser for `ci_remediation` workers.
    /// Mirrors [`Self::finalize_conflict_resolution_attempt`]. Fires for
    /// every Stop event on a `ci_remediation` execution; decides whether
    /// to mark the bound `ci_remediations` row `failed` with the
    /// catch-all reason ([`CI_NO_PUSH_REASON`]).
    ///
    /// Same rule as the conflict-resolver flow: if the attempt is still
    /// `running`, `head_sha_after IS NULL`, `failure_reason IS NULL`,
    /// AND the worker exited without pushing (PR not freshly bound),
    /// the engine has no signal that the worker classified its own
    /// outcome — default to `failed` with the catch-all reason. On
    /// `Fresh` / `Merged` outcomes the merge poller's `on_ci_resolved`
    /// retire path will mark the attempt `succeeded` shortly. On the
    /// `Stale` / `EmptyDiff` paths the on-Stop probe queue is already
    /// chasing the worker for a follow-up push, so leave the attempt
    /// alone.
    ///
    /// Idempotent — the underlying
    /// [`WorkDb::mark_ci_remediation_failed`] WHERE-guards on
    /// `status IN ('pending', 'running')`, so a duplicate finaliser
    /// call after a terminal transition writes nothing.
    pub async fn finalize_ci_remediation_attempt(&self, execution: &crate::work::WorkExecution, outcome: &StopOutcome) {
        let attempt = match self
            .work_db
            .active_ci_remediation_for_work_item(&execution.work_item_id)
        {
            Ok(Some(attempt)) => attempt,
            Ok(None) => {
                tracing::debug!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    "ci-remediation finalizer: no active attempt; nothing to do",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "ci-remediation finalizer: failed to look up active attempt",
                );
                return;
            }
        };
        // Already past the "running with no outcome" window — the
        // worker classified via `mark-failed` / `mark-retriggered`,
        // the poller already retired it, or some other path closed
        // the row. Nothing for the catch-all to do.
        if attempt.status != "running" || attempt.head_sha_after.is_some() || attempt.failure_reason.is_some() {
            return;
        }

        let should_mark_failed = match outcome {
            // Worker pushed (or the PR is already merged from this run).
            // The merge poller's on_ci_resolved retire path will mark
            // the attempt `succeeded` once CI is green.
            StopOutcome::PrDetected { .. } | StopOutcome::PrMerged { .. } => false,
            // Worker pushed something but the PR head still trails the
            // worker's local commits, or pushed an empty diff. The
            // on-Stop probe path has already nudged the worker; don't
            // pre-empt with a `failed` mark.
            StopOutcome::StalePr { .. } | StopOutcome::EmptyDiffPr { .. } => false,
            // Race with an already-finalized execution, or a stale Stop
            // from a superseded reused-workspace occupant.
            StopOutcome::AlreadyTerminal | StopOutcome::UnknownExecution | StopOutcome::SupersededInWorkspace => false,
            // Incident-001 gates (mirrors conflict finalizer).
            StopOutcome::RunningNoStagedPr => false,
            StopOutcome::FallbackDisabledByFlag => false,
            // Breaker parked the execution for a human; don't mark the
            // attempt failed (that risks a retrigger that re-loops).
            StopOutcome::NudgeBreakerParked { .. } => false,
            // Worker declared an unresolved escalation/blocker; the
            // auto-nudge is suppressed, not the attempt marked failed —
            // mirrors NudgeBreakerParked.
            StopOutcome::EscalationPending { .. } => false,
            // Worker is narrating a legitimate backgrounded build/test wait;
            // the auto-nudge is suppressed, not the attempt marked failed —
            // mirrors EscalationPending/NudgeBreakerParked.
            StopOutcome::BuildWaitPending { .. } => false,
            // Signal was already cleared before this worker ran — the
            // attempt has been marked succeeded by try_retire_cleared_blocking_signal.
            StopOutcome::SignalAlreadyCleared { .. } => false,
            // Deliverable-satisfied gate: the bound PR was already clean
            // (CI green + no conflict) or merged when the worker stopped
            // without pushing. The execution is finalized, not failed.
            StopOutcome::DeliverableSatisfied { .. } => false,
            // Worker re-triggered a flaky/infra failure — the attempt was
            // already flipped to terminal `retriggered` by mark-retriggered,
            // so there is no running row to retire here, and this is
            // explicitly NOT a failure.
            StopOutcome::FlakyRetriggered { .. } => false,
            // Unreachable here (this finalizer only runs for `ci_remediation`
            // kind), but a triage outcome must never mark a CI attempt failed.
            StopOutcome::AutomationTriage { .. } => false,
            // Unreachable here for the same reason: an answer-agent outcome
            // must never mark a CI attempt failed either.
            StopOutcome::AnswerAgent { .. } => false,
            // Unreachable: reviewer executions short-circuit before CI
            // remediation finalisation. Covered for exhaustiveness.
            StopOutcome::ReviewerEnqueued { .. }
            | StopOutcome::ReviewPassCompleted { .. }
            | StopOutcome::ReviewPassRevisionCreated { .. }
            | StopOutcome::ReviewPassAwaitingResult => false,
            // Unreachable: the no-op terminal only fires for chore/task
            // implementation kinds, never ci_remediation. A verified
            // already-done run is a success, not a failure.
            StopOutcome::NoChangesNeeded { .. } => false,
            // Catch-all branches: worker exited without evidence of a
            // push and without classifying via `mark-failed`.
            StopOutcome::AwaitingInput
            | StopOutcome::DetectorFailed
            | StopOutcome::NoWorkspace
            | StopOutcome::DbError => true,
        };
        if !should_mark_failed {
            return;
        }

        let updated = match self.work_db.mark_ci_remediation_failed(&attempt.id, CI_NO_PUSH_REASON) {
            Ok(Some(row)) => row,
            Ok(None) => {
                tracing::debug!(
                    attempt_id = %attempt.id,
                    "ci-remediation finalizer: attempt already terminal between probes",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?err,
                    "ci-remediation finalizer: failed to mark attempt failed",
                );
                return;
            }
        };

        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            attempt_id = %updated.id,
            pr_url = %updated.pr_url,
            reason = CI_NO_PUSH_REASON,
            ?outcome,
            "ci-remediation finalizer: worker exited without pushing; attempt marked failed",
        );

        self.publisher
            .publish_frontend_event_on_product(
                &updated.product_id,
                FrontendEvent::CiRemediationFailed {
                    product_id: updated.product_id.clone(),
                    work_item_id: updated.work_item_id.clone(),
                    attempt_id: updated.id.clone(),
                    pr_url: updated.pr_url.clone(),
                    failure_reason: CI_NO_PUSH_REASON.to_owned(),
                },
            )
            .await;
    }
}
