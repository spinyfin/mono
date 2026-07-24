//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    /// Wire an externally-owned [`StagedResolutionSignalCache`] into this
    /// Handle a `Stop` event for `execution_id`. Returns the outcome
    /// classification so callers can log/test what happened.
    pub async fn on_stop(&self, execution_id: &str) -> StopOutcome {
        let outcome = self.on_stop_inner(execution_id).await;
        // `ci_remediation` (retrigger-kind only; fix-kind now dispatches through
        // revision_implementation) gets the catch-all finalizer on Stop.
        if let Ok(execution) = self.work_db.get_execution(execution_id) {
            if execution.kind == ExecutionKind::CiRemediation {
                self.finalize_ci_remediation_attempt(&execution, &outcome).await;
            }
            // Conflict resolution now dispatches through
            // `revision_implementation` (the legacy `conflict_resolution`
            // kind is kept for fallback). Either kind that stops without a
            // push must retire its `conflict_resolutions` ledger row, or
            // the attempt strands `pending` forever — the stall the
            // operator sees as "revision tasks that do nothing", and the
            // reason the engine later re-mints a fresh conflict revision
            // once `main` moves again.
            if matches!(
                execution.kind,
                ExecutionKind::RevisionImplementation | ExecutionKind::ConflictResolution
            ) {
                self.finalize_conflict_resolution_attempt(&execution, &outcome).await;
            }
        }
        outcome
    }

    /// Layer-2 defence-in-depth for the staged-PR-URL primary path:
    /// return the staged URL for `execution_id` only if it actually
    /// belongs to this execution's branch.
    ///
    /// Shared by `on_stop_inner`'s primary path and `recheck_for_pr`'s
    /// mirror of it; `log_ctx` is the message prefix distinguishing the
    /// two ("stop event" / "pr-recheck"). Returns `None` when there is
    /// no staged URL, when the branch check definitively fails (the
    /// entry is evicted from the cache and
    /// `PR_RECHECK_STAGED_BRANCH_MISMATCH` incremented), or when
    /// verification fails transiently (the entry is KEPT so the next
    /// sweep can retry).
    pub(super) async fn verified_staged_pr_url(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
        log_ctx: &str,
    ) -> Option<String> {
        let staged_url = self.staged_pr_urls.get(execution_id)?;
        // `RevisionImplementation` executions push to the CHAIN ROOT's
        // existing branch, never one derived from their own execution
        // id. `expected_branch_name(execution_id, ...)` computes a
        // branch that structurally never exists for a revision, so the
        // work-item-suffix check below would always "mismatch" and
        // discard a legitimate staged URL (2026-07-14 incident,
        // exec_18c2124d2f06d768_106d: `cube pr update`'s printed URL —
        // the chain root's real PR — was dropped here, and the
        // fallthrough to the SHA-delta gate is what actually caused the
        // stall). Verify against the resolved bound PR instead: the
        // URL a compliant `cube pr update` call prints for a revision
        // IS the chain root's PR.
        let branch_ok = if execution.kind == ExecutionKind::RevisionImplementation {
            match self.resolve_bound_pr_url(execution) {
                Some(bound_url) if bound_url == staged_url => true,
                Some(bound_url) => {
                    tracing::warn!(
                        execution_id,
                        staged_pr_url = %staged_url,
                        bound_pr_url = %bound_url,
                        "pr_recheck_staged_branch_mismatch: staged PR URL does not match the revision's bound (chain root) PR; dropping staged URL",
                    );
                    PR_RECHECK_STAGED_BRANCH_MISMATCH.inc(&self.metrics);
                    self.staged_pr_urls.forget(execution_id);
                    false
                }
                None => {
                    // No bound PR resolvable (execution.pr_url not
                    // stamped and the chain-root lookup failed) — trust
                    // the staged URL rather than discard legitimate
                    // evidence; a wrong URL here would still have to
                    // pass `validate_pr_url`'s product-repo gate at
                    // staging time.
                    true
                }
            }
        } else {
            let expected_branch = expected_branch_name(
                execution_id,
                &execution.branch_naming,
                execution.worker_branch_prefix.as_deref(),
            );
            let repo_slug = parse_repo_slug(&execution.repo_remote_url);
            match repo_slug {
                Ok(ref slug) => match pr_number_from_url(&staged_url) {
                    Some(pr_num) => match self.branch_verifier.fetch_pr_head_ref(slug, pr_num).await {
                        Ok(ref head_ref) if branches_identify_same_work_item(head_ref, &expected_branch) => {
                            if head_ref.as_str() != expected_branch.as_str() {
                                tracing::info!(
                                    execution_id,
                                    staged_pr_url = %staged_url,
                                    staged_pr_branch = %head_ref,
                                    %expected_branch,
                                    "{log_ctx}: staged PR branch prefix differs from expected but the work-item suffix matches; associating (prefix-agnostic match)",
                                );
                            }
                            true
                        }
                        Ok(head_ref) => {
                            tracing::warn!(
                                execution_id,
                                staged_pr_url = %staged_url,
                                staged_pr_branch = %head_ref,
                                %expected_branch,
                                "pr_recheck_staged_branch_mismatch: staged PR work-item suffix does not match expected; dropping staged URL",
                            );
                            PR_RECHECK_STAGED_BRANCH_MISMATCH.inc(&self.metrics);
                            self.staged_pr_urls.forget(execution_id);
                            false
                        }
                        Err(err) => {
                            // Transient API failure: cannot verify this pass, but do NOT
                            // discard the staged URL. On the next merge-poller sweep the
                            // staged URL is still present and verification is retried —
                            // dropping here would strand the worker if the cold path also
                            // fails. A definitive branch-name mismatch (the Ok(head_ref)
                            // arm above) still evicts the URL immediately.
                            tracing::warn!(
                                execution_id,
                                staged_pr_url = %staged_url,
                                ?err,
                                "{log_ctx}: branch verification failed transiently; \
                                 keeping staged URL for retry on next sweep",
                            );
                            false
                        }
                    },
                    None => {
                        tracing::warn!(
                            execution_id,
                            staged_pr_url = %staged_url,
                            "{log_ctx}: cannot parse PR number from staged URL; dropping for safety",
                        );
                        self.staged_pr_urls.forget(execution_id);
                        false
                    }
                },
                Err(err) => {
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "{log_ctx}: cannot parse repo slug; dropping staged URL for safety",
                    );
                    self.staged_pr_urls.forget(execution_id);
                    false
                }
            }
        };
        branch_ok.then_some(staged_url)
    }

    pub(super) async fn on_stop_inner(&self, execution_id: &str) -> StopOutcome {
        let execution = match self.work_db.get_execution(execution_id) {
            Ok(execution) => execution,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    ?err,
                    "stop event: execution unknown — likely a non-execution worker run"
                );
                return StopOutcome::UnknownExecution;
            }
        };

        // Already completed/failed/cancelled — nothing more to do.
        if !execution.status.is_live() {
            return StopOutcome::AlreadyTerminal;
        }

        // Stale-Stop guard (reused-workspace hook leak): if a newer live
        // execution now occupies this execution's cube workspace, this
        // Stop leaked from a stale `boss-event` hook registration left in
        // the warm-cached workspace. Finalizing here would mis-attribute
        // completion to the wrong run and could release the live run's
        // re-leased workspace. Ignore it; the newest execution's own Stop
        // drives its completion. Belt-and-suspenders with
        // `worker_setup::purge_leaked_worker_hooks`, which stops the leak
        // at the source.
        match self.work_db.execution_superseded_in_workspace(&execution) {
            Ok(true) => {
                tracing::warn!(
                    execution_id,
                    cube_workspace_id = ?execution.cube_workspace_id,
                    "stop event: execution superseded by a newer live execution in the same reused workspace — ignoring stale Stop (reused-workspace hook leak)",
                );
                return StopOutcome::SupersededInWorkspace;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "stop event: superseded-in-workspace check failed; proceeding with completion",
                );
            }
        }

        // Capture whether stop_seen was already set BEFORE stamping it so the
        // SHA-delta gate below can distinguish "first stop" from "subsequent
        // stop". For revision_implementation executions the gate uses this to
        // decide whether to require explicit push evidence (already_stop_seen=true
        // means this is a multi-turn stop where a parent push could have moved
        // the head without the revision contributing).
        let already_stop_seen = self.work_db.execution_stop_seen(execution_id).unwrap_or(false);

        // Stamp the stop_seen marker so the merge poller's SHA-delta gate
        // knows at least one Stop boundary has been observed for this
        // execution. Best-effort — failure does not block the rest of
        // on_stop_inner.
        //
        // Note: waiting_human is set immediately at pane spawn
        // (PaneSpawnRunner), so it does NOT indicate a terminal worker. The
        // Stop hook fires after every assistant turn. stop_seen = true means
        // "at least one turn boundary has been observed" — not "worker is done."
        if let Err(err) = self.work_db.set_execution_stop_seen(execution_id) {
            tracing::warn!(
                execution_id,
                ?err,
                "stop event: failed to stamp stop_seen; SHA-delta recovery gate may not fire"
            );
        }

        // Maint task 6: an `automation_triage` execution never opens a PR.
        // Its Stop is resolved by the marker-protocol outcome detector
        // (`automation: task <id>` / `automation: skip — …`), not by PR
        // detection or the nudge path below. Branch out before any of that.
        if execution.kind == ExecutionKind::AutomationTriage {
            return self.finalize_automation_triage(&execution).await;
        }

        // P3b: an `answer_agent` execution never opens a PR either. Its
        // reply — if any — was already posted mid-session via
        // `CommentsPostAnswer` (the `boss comment reply` command), which
        // already completed the `answer_agent_runs` row and transitioned the
        // comment. This just finalises the execution/run rows and, if the
        // agent's session ended without ever posting a reply, resolves the
        // stranded `running` run so the comment doesn't sit in `answering`
        // forever.
        if execution.kind == ExecutionKind::AnswerAgent {
            return self.finalize_answer_agent(&execution).await;
        }

        // A `pr_review` reviewer execution never opens a PR. It reads
        // the PR diff and emits structured findings; the producing task already
        // advanced to `in_review` on PR-open, so the Stop handler just finalises
        // the reviewer execution (which also parses the ReviewResult and
        // enqueues revisions when warranted).
        if execution.kind == ExecutionKind::PrReview {
            return self.finalize_pr_review_pass(&execution).await;
        }

        // Flaky/infra retrigger park (issue #1205): a `ci_remediation`
        // worker that diagnosed the CI failure as infra and re-ran the job
        // (`mark-retriggered`) stamped the `ci_flaky_retriggered` signal on
        // the parent. There is nothing to push, so we MUST NOT fall through
        // to PR detection or the nudge loop — every probe would just
        // re-derive the same verdict and burn worker turns. Park the worker
        // awaiting the CI retry / a human decision. The merge-poller clears
        // the signal and snaps the parent to Review once CI goes green.
        if execution.kind == ExecutionKind::CiRemediation {
            match self
                .work_db
                .has_active_ci_flaky_retrigger_signal(&execution.work_item_id)
            {
                Ok(true) => {
                    let pr_url = self.resolve_bound_pr_url(&execution).unwrap_or_default();
                    tracing::info!(
                        execution_id,
                        work_item_id = %execution.work_item_id,
                        %pr_url,
                        "stop event: parent carries ci_flaky_retriggered signal — parking worker (awaiting CI retry / human decision), not nudging",
                    );
                    return StopOutcome::FlakyRetriggered { pr_url };
                }
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(
                        execution_id,
                        work_item_id = %execution.work_item_id,
                        ?err,
                        "stop event: flaky-retrigger signal check failed; proceeding with normal completion",
                    );
                }
            }
        }

        // Primary path: a PR URL was already captured from a
        // `PostToolUse` Bash hook event (`gh pr create` /
        // `gh pr view` / `gh pr edit` stdout) while the worker was
        // still running. Layer-2 defence-in-depth: verify the staged
        // PR's headRefName matches this execution's expected branch
        // before finalizing — a mismatch means the URL was captured
        // from an unrelated Bash invocation and must be discarded.
        //
        // The cold-path fallback below remains for engine-restart
        // recovery: if the engine was down when the worker ran
        // `gh pr create`, the in-memory staging cache is empty here
        // and we fall through to `detect_pr` to reconstruct the URL
        // via the GitHub API.
        if let Some(staged_url) = self
            .verified_staged_pr_url(execution_id, &execution, "stop event")
            .await
        {
            tracing::info!(
                execution_id,
                pr_url = %staged_url,
                "stop event: using PR URL captured from worker hook stream (primary path); skipping detector",
            );
            PR_URL_CAPTURE_PRIMARY_HIT.inc(&self.metrics);
            return self
                .finalize_pr_transition(
                    execution_id,
                    staged_url,
                    WorkerPrCompletionTarget::InReview,
                    "stop_staged",
                )
                .await;
        }

        // Worker escalation/blocker detection: a worker that emitted an
        // `[effort-escalation]` or `[blocked]` marker on this Stop gets
        // an attention item filed for the coordinator *before* any
        // status-gate or nudge decision below is made — `nudge_or_park`
        // consults the same store and suppresses the "produce a PR" loop
        // while the item is unresolved. Best-effort: filing failures are
        // logged loudly (see `file_worker_signal_attention`) and
        // swallowed, never block completion.
        //
        // Deliberately runs BEFORE the running-status gate below: a pane
        // worker's Stop can land while `execution.status` is still
        // `running` rather than `waiting_human` — the coordinator flips
        // it to `waiting_human` only after `PaneSpawnRunner::run_execution`
        // returns from its spawn-ack round trip, but the pane (and the
        // claude process inside it) is already live and can emit its
        // first Stop before that round trip resolves. `running` and
        // `waiting_human` are the two `ExecutionStatus::is_live()`
        // values a pane-based worker can hold at Stop; detection must
        // cover both so a marker emitted in that narrow startup window
        // isn't silently missed. Marker detection itself never touches
        // PR state, so running it ahead of the gate carries none of the
        // race the gate below guards against.
        self.detect_and_file_worker_signals(&execution).await;

        // Deferred-scope detection: a worker that deliberately narrowed
        // its task's scope and declared it via a `[deferred-scope]`
        // marker gets that recorded durably — both on the work item's
        // own description and as a coordinator-visible attention item —
        // so the deferral is a tracked decision
        // rather than a prose sentence that dies with the transcript. Unlike
        // the escalation/blocker pair above, this never suppresses the
        // "produce a PR" nudge: the worker already produced its (narrower)
        // deliverable.
        //
        // Runs BEFORE the running-status gate below for the same reason
        // `detect_and_file_worker_signals` does: it only reads the
        // transcript and records an attention item, never touches PR
        // state, so a `[deferred-scope]` marker emitted while `running`
        // (the same narrow pane-startup window, or for the whole
        // lifetime of a `pr_review` reviewer pane) is still captured
        // instead of being silently dropped — and unlike `[blocked]`,
        // a deferred-scope marker is never re-emitted on a later Stop.
        self.detect_and_record_deferred_scope(&execution).await;

        // A probe minted on an earlier Stop can still be sitting undelivered in
        // the run's pending-probe queue (e.g. a `PROBE_NO_PR` nudge whose
        // `SendToPane` failed and was requeued for retry on the next
        // Stop). `dispatch_probe_on_stop` pops whatever is queued for a
        // run on *every* Stop, independent of what this Stop's own
        // completion decision was — so without this, a stale nudge could
        // still fire on the very Stop where the worker just reported
        // `[blocked]`/`[effort-escalation]`, even though `nudge_or_park`
        // below correctly refuses to queue a *new* one. Drop any stale
        // queued probe now, before the event loop's `dispatch_probe_on_stop`
        // gets a chance to pop it.
        if self.unresolved_worker_signal_reason(&execution).is_some() {
            self.probe_queuer.clear_pending_probes(execution_id);
        }

        // AI #6 running-status gate (incident 001 §5): in Claude Code
        // the `Stop` hook fires after every assistant turn, not just
        // at worker exit. With no staged URL on a still-`running`
        // execution we MUST NOT fall through to `detect_pr` — the
        // worker is alive and any positive result would race against
        // its own in-flight push.
        //
        // Note: `waiting_human` is set immediately at pane spawn
        // (PaneSpawnRunner), NOT at worker exit — the worker is still
        // actively running turns when in `waiting_human`. This gate
        // is useful only as a coarse filter for the PR-detection
        // fallthrough below: a worker in `running` (either between its
        // `start_execution_run`/`finish_execution_run` calls, or a
        // `pr_review` reviewer pane, which the design deliberately keeps
        // in `running` — see `RunWaitState::ReviewerPaneAlive`) never
        // falls through to `detect_pr`/the nudge loop. Marker detection
        // above already ran regardless of this gate, so a `[blocked]` or
        // `[deferred-scope]` signal from a `running` worker is still filed
        // and visible to the coordinator even though this Stop parks here
        // as a no-op.
        if execution.status != ExecutionStatus::WaitingHuman {
            tracing::debug!(
                execution_id,
                status = %execution.status,
                "stop event: no staged URL and execution is not waiting_human — skipping fallback (running-status gate)",
            );
            return StopOutcome::RunningNoStagedPr;
        }

        // Resume-bounce SHA-delta gate: when the chore already has a
        // PR bound to it (`task.pr_url` populated by an earlier run's
        // on-Stop machinery), use that URL as the authoritative
        // identifier — never branch-search. If the bound PR's head
        // SHA moved since the last Stop boundary (captured in
        // `execution.pr_head_before`) AND the revision has push evidence
        // (it ran `jj git push` in this turn, or this is the first stop
        // boundary), stamp `revision_stop_contributed_head` and finalize.
        // Without push evidence on a subsequent stop, the head movement
        // is attributed to the concurrently-active parent worker and we
        // absorb the new baseline without finalizing so the revision can
        // push on its next turn.
        match self.evaluate_sha_delta_gate(execution_id, &execution).await {
            ShaDeltaGateOutcome::Contributed { pr_url, head_now } => {
                if execution.kind == ExecutionKind::RevisionImplementation {
                    let push_staged = self.staged_revision_pushes.take(execution_id);
                    // For the first stop (already_stop_seen=false) treat the push as
                    // the revision's own contribution even without an explicit push
                    // event — single-turn revisions push and stop in one turn, and the
                    // pre-first-Stop window was already guarded by recheck_for_pr.
                    // For subsequent stops (already_stop_seen=true) require push
                    // evidence, because the parent worker may have pushed between turns.
                    let is_revision_contribution = push_staged || !already_stop_seen;
                    if is_revision_contribution {
                        // Stamp the head we're about to finalize on; recheck_for_pr
                        // uses this to recover if finalization fails transiently.
                        if let Err(err) = self.work_db.set_revision_stop_contributed_head(execution_id, &head_now) {
                            tracing::warn!(
                                execution_id,
                                ?err,
                                "stop event: failed to stamp revision_stop_contributed_head; \
                                 recheck_for_pr transient-failure recovery may not fire",
                            );
                        }
                        return self
                            .finalize_pr_transition(
                                execution_id,
                                pr_url,
                                WorkerPrCompletionTarget::InReview,
                                "stop_sha_delta",
                            )
                            .await;
                    }
                    // already_stop_seen=true with no push evidence: parent pushed.
                    // Absorb the head into the baseline and fall through to the
                    // NoContribution nudge path so the revision continues working.
                    tracing::info!(
                        execution_id,
                        pr_url = %pr_url,
                        head_now = %head_now,
                        "stop event: revision SHA-delta Contributed suppressed \
                         (already_stop_seen=true, no push evidence) — parent push assumed; \
                         absorbing baseline",
                    );
                    if let Err(err) = self.work_db.set_execution_pr_head_before(execution_id, &head_now) {
                        tracing::warn!(
                            execution_id,
                            ?err,
                            "stop event: failed to absorb pr_head_before after parent-push \
                             suppression; next turn may re-trigger spuriously",
                        );
                    }
                    // Fall through to the NoContribution arm's nudge logic.
                    // We reach the nudge_or_park below directly.
                    let _pr_url_for_nudge = pr_url;
                    // Restructure: fall through by using a shared helper.
                    // Inline the NoContribution nudge path here.
                    let conflict_prefetch = match self
                        .try_retire_cleared_blocking_signal(execution_id, &execution, &_pr_url_for_nudge)
                        .await
                    {
                        BlockingSignalOutcome::Retired(outcome) => return outcome,
                        BlockingSignalOutcome::NotRetired(prefetch) => prefetch,
                    };
                    if let Some(outcome) = self
                        .try_finalize_metadata_only_fix_on_stop(execution_id, &execution, &_pr_url_for_nudge)
                        .await
                    {
                        return outcome;
                    }
                    // The parent's push is not this revision's conflict
                    // resolution — verify against GitHub before the generic
                    // nudge invites a "nothing left to do" reply.
                    let (probe_text, fingerprint) = match self
                        .conflict_revision_stop_refusal(execution_id, &execution, &_pr_url_for_nudge, conflict_prefetch)
                        .await
                    {
                        Some(refusal) => refusal,
                        None => (
                            probe_push_to_existing_pr(&_pr_url_for_nudge),
                            format!("nocontribution:{_pr_url_for_nudge}"),
                        ),
                    };
                    tracing::info!(
                        execution_id,
                        bound_pr_url = %_pr_url_for_nudge,
                        "stop event: revision absorbed parent push — nudging to push to the existing PR",
                    );
                    return self
                        .nudge_or_park(
                            &execution,
                            &probe_text,
                            &fingerprint,
                            Some(&_pr_url_for_nudge),
                            StopOutcome::AwaitingInput,
                        )
                        .await;
                }
                return self
                    .finalize_pr_transition(
                        execution_id,
                        pr_url,
                        WorkerPrCompletionTarget::InReview,
                        "stop_sha_delta",
                    )
                    .await;
            }
            ShaDeltaGateOutcome::NoContribution { pr_url, head_now: _ } => {
                // Before nudging, check whether the blocking signal (conflict /
                // CI) is already cleared — e.g. a sibling resolver fixed the
                // conflict before this run started. If so, retire the attempt
                // and finalise the execution without nudging.
                let conflict_prefetch = match self
                    .try_retire_cleared_blocking_signal(execution_id, &execution, &pr_url)
                    .await
                {
                    BlockingSignalOutcome::Retired(outcome) => return outcome,
                    BlockingSignalOutcome::NotRetired(prefetch) => prefetch,
                };
                // Positive-evidence metadata-only CI-fix gate (issue #1252):
                // a revision can legitimately finish WITHOUT moving the head
                // when it repairs a PR-description validator via
                // `gh pr edit --body` (no commit). Because we are inside the
                // on-Stop handler, this is a *real* Stop boundary — a dead /
                // cut-off worker emits no Stop hook and never reaches here.
                // If this run also produced an operator-visible PR-metadata
                // delta, record that positive evidence and finalize (now, if
                // CI is already green; otherwise the merge poller finalizes
                // it once CI goes green — see `recheck_for_pr`). Without a
                // delta we fall through to the normal nudge: head unchanged
                // AND body unchanged means the worker contributed nothing.
                if let Some(outcome) = self
                    .try_finalize_metadata_only_fix_on_stop(execution_id, &execution, &pr_url)
                    .await
                {
                    return outcome;
                }
                // GitHub-authoritative conflict gate. A merge-conflict
                // revision that pushed nothing is claiming, implicitly or
                // explicitly, that the conflict is already gone. That claim
                // is objectively checkable and the engine holds the bound PR
                // URL — so check it here, BEFORE the satisfied-deliverable
                // gate and the generic nudge, and refuse the claim outright
                // when GitHub still reports the PR conflicting (or has not
                // finished deciding). Placed ahead of the satisfied gate so
                // a refusal costs one probe round rather than two.
                let conflict_refusal = self
                    .conflict_revision_stop_refusal(execution_id, &execution, &pr_url, conflict_prefetch)
                    .await;
                if let Some((probe_text, fingerprint)) = conflict_refusal {
                    return self
                        .nudge_or_park(
                            &execution,
                            &probe_text,
                            &fingerprint,
                            Some(&pr_url),
                            StopOutcome::AwaitingInput,
                        )
                        .await;
                }
                // Deliverable-satisfied gate (zombie-worker fix): if the
                // bound PR is already in a satisfactory state at this Stop
                // boundary — CI clean and no merge conflict, or already
                // merged — the worker's deliverable is complete regardless
                // of whether it pushed new commits this run. Finalize now
                // instead of nudging, preventing the "nothing left to do"
                // spin loop where workers park in waiting_for_input and
                // hold their pool slot indefinitely.
                //
                // IMPORTANT: this gate is intentionally placed only in
                // on_stop, not in recheck_for_pr. The merge-poller sweep
                // runs for waiting_human executions even when the worker
                // died without a clean Stop (crash, API cut). Applying
                // "head unchanged + CI clean → finalize" there would reap
                // dead workers that still need reconciliation — the exact
                // race rolled back in #1262. The on_stop path is safe
                // because the Stop hook fires only when the worker
                // completed a turn (real activity boundary, not a crash).
                if let Some(outcome) = self
                    .try_finalize_satisfied_deliverable_on_stop(execution_id, &execution, &pr_url)
                    .await
                {
                    return outcome;
                }
                tracing::info!(
                    execution_id,
                    bound_pr_url = %pr_url,
                    "stop event: bound PR did not move during this run — nudging to push to the existing PR"
                );
                // A PR is already bound: never tell the worker to create
                // one. Nudge it to push to the existing branch, bounded
                // by the circuit breaker.
                return self
                    .nudge_or_park(
                        &execution,
                        &probe_push_to_existing_pr(&pr_url),
                        &format!("nocontribution:{pr_url}"),
                        Some(&pr_url),
                        StopOutcome::AwaitingInput,
                    )
                    .await;
            }
            ShaDeltaGateOutcome::Inapplicable => {
                // A `revision_implementation` execution with a resolvable
                // bound PR (via `execution.pr_url` / chain-root lookup) but
                // an inconclusive SHA-delta gate. This covers two distinct
                // causes that must be handled the same way:
                //   (a) `pr_head_before` WAS captured but today's fetch of
                //       the current head failed transiently, or
                //   (b) `pr_head_before` was NEVER captured at all — the
                //       dispatch-time snapshot in `on_execution_started`
                //       failed (or the execution predates reliable
                //       snapshotting) — so there is no baseline to compare
                //       against, ever, for the lifetime of this execution.
                // Either way we cannot tell via SHA comparison whether this
                // run contributed a new commit. The cold-path branch-keyed
                // detector always returns None for revisions (they push to
                // the parent PR's branch and never open their own), so
                // falling through to it lands on `resolve_bound_pr_url` →
                // nudge "push to existing PR". For case (b) that nudge is a
                // dead end: if the worker already pushed, there is nothing
                // new to push, so the same nudge fires on every Stop until
                // the circuit breaker trips and stamps the revision
                // permanently stuck in `waiting_human` — never reaching a
                // terminal status even though the commit landed (the
                // stuck-revision incident this branch exists to close; see
                // the sibling fix elsewhere in this module that covered only case (a)).
                //
                // Instead, fall back to the CI-state-based satisfied-
                // deliverable gate: if the bound PR is currently open with
                // clean CI and no conflict (or already merged), that is
                // direct, SHA-independent evidence the deliverable is
                // satisfied — finalize now. Safe to run from the on-Stop
                // boundary for the same reason
                // `try_finalize_satisfied_deliverable_on_stop` is safe
                // elsewhere: a Stop event only fires on real worker
                // activity, never a crash. When the PR isn't satisfied yet
                // (CI in flight/failing, or a conflict), return
                // AwaitingInput quietly — no nudge — and let the next
                // natural Stop (or a human/coordinator prompt) retry.
                // `recheck_for_pr` (the periodic merge-poller sweep) does
                // NOT run this check — it can't rule out a crashed worker —
                // so it stays gated to the on-Stop path here.
                if execution.kind == ExecutionKind::RevisionImplementation
                    && let Some(bound_pr_url) = self.resolve_bound_pr_url(&execution)
                {
                    // Same GitHub-authoritative gate as the NoContribution
                    // arm, and for the same reason: a merge-conflict
                    // revision that pushed its resolution commonly lands
                    // here with GitHub's mergeability recompute still
                    // in-flight (`mergeable: UNKNOWN`), which
                    // `mergeability_satisfies_deliverable` below never
                    // treats as satisfied for this kind. Resolve `UNKNOWN`
                    // to a definite answer (bounded retry) before falling
                    // through to the satisfied-deliverable check, so a
                    // pushed resolution does not strand in `AwaitingInput`
                    // with no nudge.
                    let conflict_refusal = self
                        .conflict_revision_stop_refusal(execution_id, &execution, &bound_pr_url, None)
                        .await;
                    if let Some((probe_text, fingerprint)) = conflict_refusal {
                        return self
                            .nudge_or_park(
                                &execution,
                                &probe_text,
                                &fingerprint,
                                Some(&bound_pr_url),
                                StopOutcome::AwaitingInput,
                            )
                            .await;
                    }
                    if let Some(outcome) = self
                        .try_finalize_satisfied_deliverable_on_stop(execution_id, &execution, &bound_pr_url)
                        .await
                    {
                        return outcome;
                    }
                    tracing::info!(
                        execution_id,
                        %bound_pr_url,
                        pr_head_before_captured = execution.pr_head_before.is_some(),
                        "stop event: revision_implementation with inconclusive SHA-delta gate and \
                         deliverable not yet satisfied — skipping cold-path nudge to avoid a \
                         push-to-existing-PR probe loop; will retry on the next Stop"
                    );
                    return StopOutcome::AwaitingInput;
                }
                // No bound `chore.pr_url` resolvable. Fall through to the
                // existing branch-keyed cold-path detector (new-PR flow).
            }
        }

        // AI #5 feature-flag gate (incident 001 §5): the cold-path
        // fallback is the path that produced the mis-binds in the
        // incident. The human can flip this off in the macOS app
        // debug pane to immediately suppress the path without a
        // rebuild. When OFF, empty staging falls through to "no PR
        // pushed" — the chore stays in `waiting_human` until the
        // human resolves it by hand.
        if !self.feature_flags.is_enabled("detect_pr_cold_fallback") {
            tracing::info!(
                execution_id,
                "stop event: detect_pr_cold_fallback flag is OFF — skipping fallback",
            );
            return StopOutcome::FallbackDisabledByFlag;
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
                // Do NOT probe the worker on a detector failure.  The failure
                // is usually a transient `gh`/network issue; probing here
                // creates a re-entrancy loop: worker receives the probe,
                // responds, stops, detection fails again, probe again…
                // The merge-poller's recheck sweep will recover the
                // transition once the failure clears.
                tracing::warn!(
                    execution_id,
                    expected_branch = %expected_branch,
                    ?err,
                    "stop event: PR detection failed; will retry on next merge-poller sweep"
                );
                PR_URL_CAPTURE_RECONSTRUCTION_FAILED.inc(&self.metrics);
                return StopOutcome::DetectorFailed;
            }
        };

        let (pr_url, target) = match pr_status {
            PrStatus::None | PrStatus::Closed { .. } => {
                // The branch-keyed detector found no PR on *this*
                // execution's branch. Before concluding "no PR, nudge to
                // create one", resolve whether the chore already has a
                // PR bound on a sibling execution (the `ci_remediation`
                // / resume case the cold-path search structurally
                // misses). If so, never say `gh pr create` — nudge to
                // push to the existing PR instead.
                if let Some(bound_pr_url) = self.resolve_bound_pr_url(&execution) {
                    tracing::info!(
                        execution_id,
                        expected_branch = %expected_branch,
                        %bound_pr_url,
                        kind = %execution.kind,
                        "stop event: chore already has a bound PR the branch search missed — nudging to push to it, not create"
                    );
                    return self
                        .nudge_or_park(
                            &execution,
                            &probe_push_to_existing_pr(&bound_pr_url),
                            &format!("push_existing:{bound_pr_url}"),
                            Some(&bound_pr_url),
                            StopOutcome::AwaitingInput,
                        )
                        .await;
                }
                // No bound PR resolvable. A `ci_remediation` worker must
                // NEVER be told to create a PR — if it somehow has no
                // bound PR, that is an anomalous upstream state; park it
                // for a human rather than nudging it to `gh pr create`.
                if execution.kind == ExecutionKind::CiRemediation {
                    tracing::warn!(
                        execution_id,
                        kind = %execution.kind,
                        "stop event: ci_remediation execution has no resolvable bound PR — parking instead of nudging to create one"
                    );
                    return self
                        .park_for_unproductive_nudges(
                            &execution,
                            0,
                            None,
                            "ci_remediation execution has no bound PR to push to; it must not be \
asked to open one",
                        )
                        .await;
                }
                // `revision_implementation` workers must NEVER be told to
                // create a PR — their deliverable is a commit on the parent
                // task's existing PR branch.  The chain-root lookup above
                // covers the common case; if we still have no resolvable PR
                // it is an upstream data anomaly.  Park for a human instead
                // of contradicting the worker's own task instructions.
                if execution.kind == ExecutionKind::RevisionImplementation {
                    tracing::warn!(
                        execution_id,
                        kind = %execution.kind,
                        "stop event: revision_implementation execution has no resolvable bound PR — parking instead of nudging to create one"
                    );
                    return self
                        .park_for_unproductive_nudges(
                            &execution,
                            0,
                            None,
                            "revision_implementation execution has no bound PR to push to; it \
must not be asked to open one",
                        )
                        .await;
                }
                // Sanctioned no-op terminal: a primary-implementation
                // worker (chore / task) that investigated and found the work
                // ALREADY DONE — the change is already on `main`, `jj diff -r @`
                // is empty, nothing to commit/push. We are at a real Stop
                // boundary (`waiting_human`), there is no PR on this branch
                // (PrStatus::None) and none bound to the chore (the
                // resolve_bound_pr_url branch above returned), so the structural
                // state confirms an empty contribution. If the worker emitted the
                // sanctioned NO_CHANGES_NEEDED marker, this is a SUCCESS, not a
                // failure to be nudged: close the task as done without a PR.
                //
                // Requiring the explicit marker is what distinguishes "verified
                // already done" from "gave up without trying": a worker that
                // stopped with no marker still falls through to the legitimate
                // produce-a-PR nudge below (and the breaker that bounds it). We
                // must NOT globally suppress that nudge, and we must NOT push an
                // empty PR — both are the band-aids the incident forbids.
                if should_enqueue_reviewer_for_primary(&execution.kind)
                    && self.worker_signalled_no_op(execution_id).await
                {
                    tracing::info!(
                        execution_id,
                        expected_branch = %expected_branch,
                        kind = %execution.kind,
                        "stop event: worker emitted NO_CHANGES_NEEDED with no PR produced — \
                         work already done; closing task as a no-op (no PR, no nudge)"
                    );
                    return self.finalize_no_op_completion(&execution).await;
                }
                tracing::info!(
                    execution_id,
                    expected_branch = %expected_branch,
                    "stop event: worker idle without an active PR — probing to push and open one"
                );
                return self
                    .nudge_or_park(&execution, PROBE_NO_PR, "no_pr", None, StopOutcome::AwaitingInput)
                    .await;
            }
            PrStatus::Stale { url, reason } => {
                tracing::info!(
                    execution_id,
                    expected_branch = %expected_branch,
                    pr_url = %url,
                    %reason,
                    "stop event: PR exists but local commits are unpushed — probing to push"
                );
                return self
                    .nudge_or_park(
                        &execution,
                        PROBE_STALE_PR,
                        &format!("stale:{url}"),
                        Some(&url),
                        StopOutcome::StalePr {
                            pr_url: url.clone(),
                            reason,
                        },
                    )
                    .await;
            }
            PrStatus::EmptyDiff { url } => {
                tracing::warn!(
                    execution_id,
                    expected_branch = %expected_branch,
                    pr_url = %url,
                    "stop event: PR has an empty diff — worker pushed a no-op change; probing to fix or close"
                );
                return self
                    .nudge_or_park(
                        &execution,
                        PROBE_EMPTY_PR,
                        &format!("empty:{url}"),
                        Some(&url),
                        StopOutcome::EmptyDiffPr { pr_url: url.clone() },
                    )
                    .await;
            }
            PrStatus::Fresh { url } => (url, WorkerPrCompletionTarget::InReview),
            PrStatus::Merged { url } => (url, WorkerPrCompletionTarget::Done),
        };
        self.finalize_pr_transition(execution_id, pr_url, target, "stop").await
    }
}
