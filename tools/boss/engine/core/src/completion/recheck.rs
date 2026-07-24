//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    /// Periodic fallback for the merge poller. Re-runs PR detection
    /// against `execution_id` and transitions the work item on a
    /// `Fresh` / `Merged` result, but stays QUIET on the no-PR /
    /// stale-PR / detector-failure branches — the on-Stop probe
    /// queueing and `worker_awaiting_pr` publish only make sense as a
    /// one-shot response to a Stop event. A 60s poller calling
    /// `on_stop` would (a) spam the worker's probe FIFO with
    /// duplicate "push your branch" messages every minute and
    /// (b) publish a steady stream of `worker_awaiting_pr` events
    /// while the worker sat idle. `recheck_for_pr` exists so the
    /// poller can drive the success path without the side effects.
    ///
    /// Closes the missed-PR-open window: if the on-Stop hook fired
    /// before GitHub's `commits/{sha}/pulls` index caught up with a
    /// freshly-created PR (the typical 7-second window observed in
    /// PR #415), this sweep picks the chore up on the next pass and
    /// completes the `active → in_review` transition.
    pub async fn recheck_for_pr(&self, execution_id: &str) -> StopOutcome {
        let execution = match self.work_db.get_execution(execution_id) {
            Ok(execution) => execution,
            Err(_) => return StopOutcome::UnknownExecution,
        };
        if !execution.status.is_live() {
            return StopOutcome::AlreadyTerminal;
        }
        // P992 task 7: reviewer executions never open a PR; skip the
        // PR-detection recheck entirely. The reviewer's Stop path already
        // drives its resolution via finalize_pr_review_pass.
        if execution.kind == ExecutionKind::PrReview {
            return StopOutcome::AlreadyTerminal;
        }
        // Primary path mirror: if the PostToolUse dispatcher already
        // captured this execution's PR URL from the worker's hook
        // stream, finalize via that URL and skip the detector. Layer-2
        // defence-in-depth: verify the staged PR's headRefName matches
        // this execution's expected branch before trusting the URL. A
        // mismatch means the URL was captured from an unrelated Bash
        // invocation (e.g. reading a chore description that referenced
        // an old PR number) and must be discarded.
        if let Some(staged_url) = self
            .verified_staged_pr_url(execution_id, &execution, "pr-recheck")
            .await
        {
            tracing::info!(
                execution_id,
                pr_url = %staged_url,
                "pr-recheck: using PR URL captured from worker hook stream (primary path); skipping detector",
            );
            PR_URL_CAPTURE_PRIMARY_HIT.inc(&self.metrics);
            return self
                .finalize_pr_transition(
                    execution_id,
                    staged_url,
                    WorkerPrCompletionTarget::InReview,
                    "pr_recheck_staged",
                )
                .await;
        }

        // Running-status gate mirror (AI #6): the merge-poller's
        // recheck sweep is intended for `waiting_human` workers whose
        // staged URL was missed. Skipping for `running` keeps the
        // fallback off in-flight workers even when the poller's
        // candidate query picks them up by race.
        //
        // Note: waiting_human is set immediately at pane spawn
        // (PaneSpawnRunner), so it does NOT indicate a terminal worker.
        // The check is still useful as a coarse filter because `running`
        // executions are between their start_execution_run and
        // finish_execution_run calls and are never WaitingHuman.
        if execution.status != ExecutionStatus::WaitingHuman {
            tracing::debug!(
                execution_id,
                status = %execution.status,
                "pr-recheck: skipping fallback — execution is not waiting_human (running-status gate)",
            );
            return StopOutcome::RunningNoStagedPr;
        }

        // SHA-delta gate: for executions with a bound PR URL — either the
        // task's own `pr_url` (chore resume) or `execution.pr_url` for
        // `revision_implementation` tasks (which never open their own PR but
        // push commits to the parent PR) — check whether the bound PR's HEAD
        // SHA moved since the last Stop boundary.
        //
        // This is the primary recovery path for `revision_implementation`
        // executions: the cold-path branch-keyed detector always returns None
        // for revisions because they have no branch of their own. The SHA-delta
        // gate is therefore the only fallback that can advance a revision from
        // `active` to `in_review` when `on_stop_inner` failed transiently (T848).
        //
        // For `revision_implementation` executions the `Contributed` arm uses
        // `revision_stop_contributed_head` (stamped by `on_stop_inner` when it
        // confirmed the revision's own push) as the finalization gate:
        //
        // - `head_now == revision_stop_contributed_head` → T848 recovery: the
        //   revision pushed this exact head, on_stop_inner attempted finalization
        //   but failed transiently; retry now.
        // - `head_now != revision_stop_contributed_head` (including NULL) → head
        //   moved from a *different* worker (parent chore's concurrent push);
        //   absorb the new baseline so subsequent sweeps don't re-trigger.
        //
        // Non-revision executions (chore resumes) finalize unconditionally on
        // `Contributed`. `NoContribution` is exempt from all of the above — the
        // metadata-only CI-fix path (issue #1252) has its own separate gate.
        match self.evaluate_sha_delta_gate(execution_id, &execution).await {
            ShaDeltaGateOutcome::Contributed { pr_url, head_now }
                if execution.kind == ExecutionKind::RevisionImplementation =>
            {
                let committed_head = self
                    .work_db
                    .get_revision_stop_contributed_head(execution_id)
                    .unwrap_or(None);
                if committed_head.as_deref() == Some(head_now.as_str()) {
                    // T848 recovery: on_stop_inner confirmed this head was the
                    // revision's own push; finalize now.
                    tracing::info!(
                        execution_id,
                        pr_url = %pr_url,
                        head_now = %head_now,
                        "pr-recheck: revision_stop_contributed_head matches current head — \
                         finalising (T848 recovery)",
                    );
                    return self
                        .finalize_pr_transition(
                            execution_id,
                            pr_url,
                            WorkerPrCompletionTarget::InReview,
                            "pr_recheck_sha_delta",
                        )
                        .await;
                }
                // Head moved but revision_stop_contributed_head doesn't
                // match (or was never set). This could be a genuine
                // parent-worker push — OR the revision's own worker is
                // still actively running and simply hasn't reached its own
                // Stop boundary yet: `execution.status` is `waiting_human`
                // for the worker's ENTIRE session, not just once it goes
                // idle (PaneSpawnRunner sets it at pane spawn), so this
                // periodic sweep can land between the worker's push and its
                // own Stop event. Do NOT mutate `pr_head_before` here —
                // 2026-07-14 incident (T342 / exec_18c2124d2f06d768_106d):
                // a poller sweep raced a live worker's in-flight push,
                // absorbed the just-pushed head as the new baseline here,
                // and when the worker's own on_stop ran moments later its
                // SHA-delta gate saw head_now == pr_head_before and
                // produced a false NoContribution — stranding the revision
                // in `active` forever, since no future delta could ever be
                // observed again. Only `on_stop_inner`'s own
                // already_stop_seen-gated absorption is trustworthy: it
                // runs at a turn boundary the worker itself just crossed.
                // Leave the baseline untouched and defer to the worker's
                // own next Stop; a genuinely dead/abandoned revision with
                // no worker left to Stop is a liveness question for a
                // different reconciler, not this sweep.
                tracing::debug!(
                    execution_id,
                    pr_url = %pr_url,
                    head_now = %head_now,
                    committed_head = ?committed_head,
                    "pr-recheck: revision Contributed unattributed — deferring to the worker's \
                     own Stop boundary rather than absorbing a possibly-in-flight push as baseline",
                );
                return StopOutcome::AwaitingInput;
            }
            ShaDeltaGateOutcome::Contributed { pr_url, head_now: _ } => {
                tracing::info!(
                    execution_id,
                    pr_url = %pr_url,
                    "pr-recheck: SHA-delta gate: bound PR head moved since last Stop — \
                     finalising without cold-path detector",
                );
                return self
                    .finalize_pr_transition(
                        execution_id,
                        pr_url,
                        WorkerPrCompletionTarget::InReview,
                        "pr_recheck_sha_delta",
                    )
                    .await;
            }
            ShaDeltaGateOutcome::NoContribution { pr_url, head_now: _ } => {
                // Bound PR did not advance during this run. For most resumes
                // the cold-path detector below returns quietly for revisions
                // and the next sweep retries, waiting for a push that moves
                // the head.
                //
                // The one exception is a legitimate PR-metadata-only CI fix
                // (issue #1252): a revision that repaired a PR-description
                // validator with `gh pr edit --body` makes no commit, so the
                // head never moves and CI can go green *after* the worker
                // stopped — past the last Stop event, so `on_stop` can no
                // longer finalize it. The merge poller is the only path that
                // can. But — unlike the rolled-back #1262 gate — we do NOT
                // infer "done" from "head unchanged + CI green": that race
                // reaped live and dead workers alike. We finalize ONLY when
                // `on_stop` already stamped the positive-evidence marker
                // (a real Stop boundary observed an operator-visible PR-body
                // delta) AND CI is now green. A dead/cut-off worker never
                // reaches a clean Stop, so it never carries the marker and is
                // never finalized here — it falls through and is surfaced /
                // re-dispatched by the normal incomplete-execution paths.
                if execution.kind == ExecutionKind::RevisionImplementation
                    && self
                        .work_db
                        .execution_metadata_fix_confirmed(execution_id)
                        .unwrap_or(false)
                    && let Some(outcome) = self
                        .finalize_metadata_only_revision_if_ready(execution_id, &pr_url)
                        .await
                {
                    return outcome;
                }
            }
            ShaDeltaGateOutcome::Inapplicable => {
                // No bound PR or snapshot unavailable — fall through to the
                // cold-path branch-keyed detector.
            }
        }

        // Feature-flag gate mirror (AI #5): the merge-poller's sweep
        // runs on the same cold-path fallback `on_stop_inner` does,
        // so the human's debug-pane toggle must take effect here too.
        if !self.feature_flags.is_enabled("detect_pr_cold_fallback") {
            tracing::debug!(
                execution_id,
                "pr-recheck: detect_pr_cold_fallback flag is OFF — skipping fallback",
            );
            return StopOutcome::FallbackDisabledByFlag;
        }

        // A `revision_implementation` execution pushes to the CHAIN ROOT's
        // existing branch, never one derived from its own execution id —
        // the branch-keyed cold-path detector below can structurally never
        // find a match for it (`query_pr_by_branch_suffix` scans up to the
        // 100-PR API cap looking for a branch that can never exist). Every
        // inconclusive sweep landed here and burned that futile scan before
        // reaching the exact same `AwaitingInput` outcome a quiet skip
        // reaches directly (2026-07-14 incident, T342 /
        // exec_18c2124d2f06d768_106d — the poller looped this scan every
        // sweep until an unrelated recovery path noticed the PR had
        // merged). Skip straight to that outcome instead.
        if execution.kind == ExecutionKind::RevisionImplementation {
            tracing::debug!(
                execution_id,
                "pr-recheck: skipping branch-keyed cold-path detector for a revision — it can \
                 never match a branch of its own; awaiting the worker's next Stop",
            );
            return StopOutcome::AwaitingInput;
        }

        let expected_branch = expected_branch_name(
            &execution.id,
            &execution.branch_naming,
            execution.worker_branch_prefix.as_deref(),
        );
        PR_URL_CAPTURE_RECONSTRUCTION_HIT.inc(&self.metrics);
        let pr_status = match self
            .pr_detector
            .detect_pr(&execution.repo_remote_url, &expected_branch)
            .await
        {
            Ok(value) => value,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    expected_branch = %expected_branch,
                    ?err,
                    "pr-recheck: detector failed; will retry next sweep"
                );
                PR_URL_CAPTURE_RECONSTRUCTION_FAILED.inc(&self.metrics);
                return StopOutcome::DetectorFailed;
            }
        };
        let (pr_url, target) = match pr_status {
            // Quiet returns — no probes, no awaiting-input publish.
            PrStatus::None | PrStatus::Closed { .. } => return StopOutcome::AwaitingInput,
            PrStatus::Stale { url, reason } => return StopOutcome::StalePr { pr_url: url, reason },
            PrStatus::EmptyDiff { url } => return StopOutcome::EmptyDiffPr { pr_url: url },
            PrStatus::Fresh { url } => (url, WorkerPrCompletionTarget::InReview),
            PrStatus::Merged { url } => (url, WorkerPrCompletionTarget::Done),
        };
        self.finalize_pr_transition(execution_id, pr_url, target, "pr_recheck")
            .await
    }

    /// PR-detection recheck for a terminal execution (status
    /// `abandoned`, `completed`, or `failed`) whose task is still
    /// `active` with no `pr_url`. This is the Bug B recovery path for
    /// the double-spawn race: exec_A is abandoned by the orphan sweep,
    /// exec_A's pane later pushes a PR, and the on-Stop hook returns
    /// `AlreadyTerminal` because exec_A is already in a terminal status.
    ///
    /// Unlike [`Self::recheck_for_pr`] this method does **not** gate on
    /// execution status and does **not** call
    /// `record_worker_pr_completion` (which requires `running` /
    /// `waiting_human`). Instead, on a `Fresh` PR detection, it calls
    /// [`WorkDb::bind_pr_to_active_task_from_terminal_execution`] to
    /// advance only the task row.
    pub async fn recheck_for_pr_late(&self, candidate: &crate::work::LatePrCandidate) -> StopOutcome {
        let expected_branch = expected_branch_name(
            &candidate.execution_id,
            &candidate.branch_naming,
            candidate.worker_branch_prefix.as_deref(),
        );
        let pr_status = match self
            .pr_detector
            .detect_pr(&candidate.repo_remote_url, &expected_branch)
            .await
        {
            Ok(value) => value,
            Err(err) => {
                tracing::debug!(
                    execution_id = %candidate.execution_id,
                    expected_branch = %expected_branch,
                    ?err,
                    "pr-recheck-late: detector failed; will retry next sweep"
                );
                return StopOutcome::DetectorFailed;
            }
        };
        let pr_url = match pr_status {
            PrStatus::None | PrStatus::Closed { .. } => return StopOutcome::AwaitingInput,
            PrStatus::Stale { url, reason } => return StopOutcome::StalePr { pr_url: url, reason },
            PrStatus::EmptyDiff { url } => return StopOutcome::EmptyDiffPr { pr_url: url },
            PrStatus::Fresh { url } | PrStatus::Merged { url } => url,
        };
        match self
            .work_db
            .bind_pr_to_active_task_from_terminal_execution(&candidate.work_item_id, &pr_url)
        {
            Ok(true) => {
                tracing::info!(
                    execution_id = %candidate.execution_id,
                    work_item_id = %candidate.work_item_id,
                    pr_url = %pr_url,
                    "pr-recheck-late: bound late PR to active task (double-spawn recovery)",
                );
                StopOutcome::PrDetected { pr_url }
            }
            Ok(false) => StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(
                    execution_id = %candidate.execution_id,
                    work_item_id = %candidate.work_item_id,
                    ?err,
                    "pr-recheck-late: DB update failed"
                );
                StopOutcome::DbError
            }
        }
    }
}
