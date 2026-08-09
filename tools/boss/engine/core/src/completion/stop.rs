//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    /// Handle a worker turn ending for `execution_id`. Returns the outcome
    /// classification so callers can log/test what happened.
    ///
    /// The trigger is the **driver-supplied** turn boundary: the sole caller,
    /// `app::worker_events::dispatch_completion_on_stop`, gates on
    /// [`crate::driver::AgentDriver::turn_boundary`] rather than on the
    /// `WorkerEvent::Stop` variant that exists only because Claude Code fires
    /// a `Stop` hook into the `boss-event` shim. A driver whose turn ends via
    /// some other channel (Codex's native `turn.completed`) reaches this
    /// handler through that seam, unchanged.
    ///
    /// The `on_stop` / [`StopOutcome`] naming is deliberately kept: it is the
    /// vocabulary of the whole completion subsystem and its call sites, and
    /// renaming it would churn far more than it clarifies.
    pub async fn on_stop(&self, execution_id: &str) -> StopOutcome {
        self.on_stop_with_turn_end(execution_id, None).await
    }

    /// Same as [`Self::on_stop`], but additionally takes the driver-resolved
    /// [`crate::driver::TurnEnd`] for the Stop that triggered this call —
    /// the production call site
    /// ([`crate::app::worker_events::dispatch_completion_on_stop`]) always
    /// has one via [`crate::events_socket::IncomingHookEvent::turn_boundary`];
    /// `on_stop` passes `None` for every existing (pre-Codex) caller that
    /// has no such signal to offer, which preserves their behaviour exactly.
    ///
    /// `turn_end` is consulted only for
    /// [`boss_protocol::StopReason::Other`] — the driver's own signal that
    /// this Stop is an unrecoverable error, not a clean completion or an
    /// awaiting-input pause — see `on_stop_inner`'s early gate.
    pub async fn on_stop_with_turn_end(
        &self,
        execution_id: &str,
        turn_end: Option<&crate::driver::TurnEnd>,
    ) -> StopOutcome {
        self.on_stop_with_turn_end_deferrable(execution_id, turn_end, false)
            .await
    }

    /// Same as [`Self::on_stop_with_turn_end`], but lets the caller withhold
    /// only the terminal PR-detection/nudge/park decision inside
    /// `on_stop_inner` — everything upstream of that decision (the
    /// stale-Stop guard, the `stop_seen` stamp, `StopReason::Other`
    /// handling, and the kind-specific finalizers) still runs.
    ///
    /// `defer_finalization` is `true` only when the pre-completion probe
    /// pass in
    /// [`crate::app::worker_events::dispatch_worker_event_fanout`] delivered
    /// a probe for this same boundary — see that function's doc comment for
    /// why the terminal decision (and only the terminal decision) must wait
    /// for the turn the delivered probe earns the worker.
    pub(crate) async fn on_stop_with_turn_end_deferrable(
        &self,
        execution_id: &str,
        turn_end: Option<&crate::driver::TurnEnd>,
        defer_finalization: bool,
    ) -> StopOutcome {
        let outcome = self.on_stop_inner(execution_id, turn_end, defer_finalization).await;
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

    /// Best-effort: resolve the PR head the given staged URL is about to be
    /// finalized against and stamp `revision_stop_contributed_head`, so
    /// `recheck_for_pr`'s exact-head recovery gate has evidence to recover
    /// from if this Stop's own `finalize_pr_transition` write fails
    /// transiently. Mirrors the stamp the SHA-delta arm performs for its own
    /// path (below in this module); the staged-URL arm needs its own copy
    /// because — for a revision — it is the dominant Stop path and the
    /// SHA-delta arm is never reached from it. Failures here are logged and
    /// swallowed: this is a best-effort recovery aid, not a precondition for
    /// finalizing.
    async fn stamp_revision_stop_contributed_head_from_staged_url(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
        staged_url: &str,
    ) {
        let repo_slug = match parse_repo_slug(&execution.repo_remote_url) {
            Ok(slug) => slug,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "stop event: cannot parse repo slug; revision_stop_contributed_head not stamped",
                );
                return;
            }
        };
        let Some(pr_number) = pr_number_from_url(staged_url) else {
            tracing::warn!(
                execution_id,
                staged_pr_url = %staged_url,
                "stop event: cannot parse PR number from staged URL; revision_stop_contributed_head not stamped",
            );
            return;
        };
        let head_now = match self.branch_verifier.fetch_pr_head_oid(&repo_slug, pr_number).await {
            Ok(oid) => oid,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    staged_pr_url = %staged_url,
                    ?err,
                    "stop event: failed to fetch PR head for revision_stop_contributed_head stamp",
                );
                return;
            }
        };
        if let Err(err) = self.work_db.set_revision_stop_contributed_head(execution_id, &head_now) {
            tracing::warn!(
                execution_id,
                ?err,
                "stop event: failed to stamp revision_stop_contributed_head; \
                 recheck_for_pr transient-failure recovery may not fire",
            );
        }
    }

    /// Stage a PR URL the worker delivered over the **structured-output file
    /// contract** — the driver-agnostic primary channel (design: agent-driver
    /// abstraction §1.6). Returns `true` when a URL was newly staged.
    ///
    /// The artifact is read and gated exactly like a hook-captured URL
    /// (product-repo check here, branch verification in
    /// [`Self::verified_staged_pr_url`]), then handed to the same staging
    /// cache, so everything downstream is identical no matter which channel
    /// produced it. First-writer-wins is preserved: a URL the `PostToolUse`
    /// stream already captured from `gh pr create`'s own stdout is stronger
    /// evidence than a file the worker wrote by hand, and stays.
    pub(super) fn stage_pr_url_from_artifact(&self, execution: &crate::work::WorkExecution) -> bool {
        let payload = match crate::structured_output::read_json::<crate::structured_output::PrUrlPayload>(
            &self.structured_output_dir,
            &execution.id,
            crate::structured_output::StructuredOutputKind::PrUrl,
        ) {
            Ok(Some(payload)) => payload,
            Ok(None) => return false,
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    error = %err,
                    "pr_url_capture: PR-URL artifact present but did not validate; ignoring",
                );
                return false;
            }
        };
        if !self.stage_validated_pr_url(execution, &payload.pr_url, "structured-output artifact") {
            return false;
        }
        PR_URL_CAPTURE_ARTIFACT_HIT.inc(&self.metrics);
        true
    }

    /// Stage a PR URL recovered from the worker's prose by the **run's own
    /// driver's** fallback producer — for Claude, the "print the PR URL on
    /// its own line as the final thing in your final response" convention.
    /// Returns `true` when a URL was newly staged.
    ///
    /// Consulted only after the artifact and the hook stream have both come up
    /// empty, and only for a parked worker, because it costs a transcript read
    /// and the alternative it competes with is the far more expensive cold-path
    /// `detect_pr` reconstruction.
    async fn stage_pr_url_from_driver_prose(&self, execution: &crate::work::WorkExecution) -> bool {
        let (driver, transcript) = self.read_final_triage_message_with_driver(&execution.id).await;
        let Some(text) = transcript.into_message() else {
            return false;
        };
        let candidates = crate::driver_transcript::driver_or_default(driver.as_deref())
            .structured_output_fallback(crate::structured_output::StructuredOutputKind::PrUrl, &text);
        let staged = candidates
            .iter()
            .filter_map(|c| serde_json::from_str::<crate::structured_output::PrUrlPayload>(&c.payload).ok())
            .any(|payload| self.stage_validated_pr_url(execution, &payload.pr_url, "driver final-message producer"));
        if staged {
            PR_URL_CAPTURE_DRIVER_FALLBACK_HIT.inc(&self.metrics);
        }
        staged
    }

    /// Gate a **self-reported** `pr_url` (artifact or final message) and stage
    /// it. Returns `true` only when this call is what put a URL in the cache.
    ///
    /// Two gates, then the shared staging cache:
    ///
    /// 1. **Never for a revision.** A `revision_implementation` worker pushes
    ///    to the chain root's existing PR, so it can name that URL truthfully
    ///    while having pushed nothing — and a staged URL finalises the
    ///    revision. Push evidence for a revision comes from the SHA-delta gate
    ///    comparing GitHub's head SHA, which is exactly the "revision tasks
    ///    that do nothing" stall these channels must not reopen. The
    ///    `PostToolUse` capture is different in kind: it sees a real
    ///    `cube pr update` invocation, not a claim about one.
    /// 2. **Product-repo gate**, mirroring the `PostToolUse` capture path: a
    ///    worker running tests can emit fixture URLs, and without this check
    ///    those would bind to the work item as if they were real PRs.
    fn stage_validated_pr_url(&self, execution: &crate::work::WorkExecution, pr_url: &str, source: &str) -> bool {
        if execution.kind == ExecutionKind::RevisionImplementation {
            tracing::debug!(
                execution_id = %execution.id,
                pr_url = %pr_url,
                source,
                "pr_url_capture: ignoring self-reported URL for a revision — push evidence comes from the SHA-delta gate",
            );
            return false;
        }
        if let Err(reason) = crate::pr_url_capture::validate_pr_url(pr_url, &execution.repo_remote_url) {
            tracing::info!(
                execution_id = %execution.id,
                rejected_url = %pr_url,
                %reason,
                source,
                "pr_url_capture: dropping URL — failed product-repo gate",
            );
            return false;
        }
        // Artifact / driver-prose URLs are self-reported publish evidence and
        // must arm finalization. Use the arming path so a prior binding-only
        // observation (`gh pr view`) cannot leave the entry permanently
        // unarmed — `record_if_unset` would return AlreadyStaged and change
        // nothing in that case.
        match self
            .staged_pr_urls
            .record_command_observation(&execution.id, pr_url, true)
        {
            crate::pr_url_capture::RecordCommandObservationOutcome::Bound
            | crate::pr_url_capture::RecordCommandObservationOutcome::Armed => {
                tracing::info!(
                    execution_id = %execution.id,
                    pr_url = %pr_url,
                    source,
                    "pr_url_capture: staged PR URL",
                );
                true
            }
            crate::pr_url_capture::RecordCommandObservationOutcome::Unchanged => {
                tracing::debug!(
                    execution_id = %execution.id,
                    pr_url = %pr_url,
                    source,
                    "pr_url_capture: ignoring URL (one is already staged for this execution)",
                );
                false
            }
        }
    }

    pub(super) async fn on_stop_inner(
        &self,
        execution_id: &str,
        turn_end: Option<&crate::driver::TurnEnd>,
        defer_finalization: bool,
    ) -> StopOutcome {
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

        // Driver-reported fatal error (StopReason::Other): the run's driver
        // — today, Codex's rollout `task_complete.error` or a stdout
        // `turn.failed` / fatal `error` envelope — has told us this Stop is
        // not a clean completion, an interruption, or an awaiting-input
        // pause, but an unrecoverable error the worker process already
        // exited on. Fail loudly here, before ANY kind-specific finalizer
        // (automation triage, answer-agent, pr_review) or the generic nudge
        // loop below gets a chance to treat the dead process as idle and
        // either wait for a result it can never produce or re-prompt a pane
        // that is no longer there. This is the fix for the incident where a
        // codex pr_review worker died on an HTTP 400, the engine read the
        // clean turn boundary as a normal idle worker, and nudged (and kept
        // renudging) an already-exited process while its cube lease leaked.
        if turn_end.map(|end| end.reason) == Some(boss_protocol::StopReason::Other) {
            let detail = self
                .read_final_triage_message(execution_id)
                .await
                .into_message()
                .unwrap_or_else(|| {
                    "the driver reported a fatal error but no diagnostic text was recovered from \
                     the transcript"
                        .to_owned()
                });
            tracing::error!(
                execution_id,
                kind = %execution.kind,
                detail,
                "stop event: driver reported this turn boundary as an unrecoverable error; \
                 failing the execution instead of nudging a dead process",
            );
            return self.finalize_driver_terminal_error(&execution, &detail).await;
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

        // Deferred-for-probe-turn: the pre-completion probe pass delivered a
        // probe into the pane for this same boundary, so the worker is
        // entitled to a turn to act on it before the generic
        // PR-detection/nudge/park decision below runs — that decision is
        // re-evaluated fresh at the `Stop` the granted turn produces.
        // Everything above this point (the stale-Stop guard, the
        // `stop_seen` stamp, `StopReason::Other` handling, and every
        // kind-specific finalizer) has already run unconditionally for this
        // boundary; only the terminal decision below is withheld. See
        // [`crate::app::worker_events::dispatch_worker_event_fanout`]'s doc
        // comment for the full rationale.
        if defer_finalization {
            tracing::info!(
                execution_id,
                "stop event: deferring the PR-detection/nudge/park decision to the Stop the \
                 just-delivered probe's turn produces",
            );
            return StopOutcome::DeferredForProbeTurn;
        }

        // Primary channel: the structured-output PR-URL artifact the worker
        // wrote (file contract — no transcript or hook-stream knowledge
        // needed, so every driver can satisfy it). It fills the same staging
        // cache the hook path does, and loses to an already-staged URL, so
        // this only takes effect where the hook stream produced nothing.
        self.stage_pr_url_from_artifact(&execution);

        // Next: a PR URL already captured from a `PostToolUse` Bash hook
        // event while the worker was still running — but only when that
        // observation was *finalization-armed* (publish/push evidence:
        // `gh pr create`, `cube pr create|update|ensure`, `jj git push`).
        // A bare `gh pr view|list|edit` binds the URL without arming, so
        // Stop must not finalize a revision that only inspected its parent
        // PR and pushed nothing. Layer-2 defence-in-depth: verify the staged
        // PR's headRefName matches this execution's expected branch before
        // finalizing — a mismatch means the URL was captured from an
        // unrelated Bash invocation and must be discarded.
        //
        // The cold-path fallback below remains for engine-restart
        // recovery: if the engine was down when the worker ran
        // `gh pr create`, the in-memory staging cache is empty here
        // and we fall through to `detect_pr` to reconstruct the URL
        // via the GitHub API.
        let staged_armed = self
            .staged_pr_urls
            .get_entry(execution_id)
            .is_none_or(|entry| entry.finalization_armed);
        if staged_armed
            && let Some(staged_url) = self
                .verified_staged_pr_url(execution_id, &execution, "stop event")
                .await
        {
            tracing::info!(
                execution_id,
                pr_url = %staged_url,
                "stop event: using PR URL captured from worker hook stream (primary path); skipping detector",
            );
            PR_URL_CAPTURE_PRIMARY_HIT.inc(&self.metrics);
            // This arm is the dominant Stop path for a revision (the
            // prompt has it run `gh pr edit`/`gh pr view`, both of which
            // stage a URL), so it must stamp
            // `revision_stop_contributed_head` itself rather than relying
            // on the SHA-delta arm below, which this staged-URL return
            // never reaches. Without this stamp, a transient
            // `finalize_pr_transition` failure here strands the revision:
            // `recheck_for_pr` defers a revision's retained staged URL only
            // while its worker is observed mid-turn (bounded by
            // `DEFAULT_STAGED_PR_MID_TURN_DEFER_SECS`), so once that horizon
            // expires it needs the stamped head to recover promptly rather
            // than finalizing against stale evidence.
            if execution.kind == ExecutionKind::RevisionImplementation {
                self.stamp_revision_stop_contributed_head_from_staged_url(execution_id, &execution, &staged_url)
                    .await;
                // incident-004 AI-3: staged URL + publish arm is not enough.
                // If the contribution gate refuses (sha_unchanged, no
                // metadata, no explicit no-op), fall through to the SHA-delta
                // / no-op / nudge path rather than returning a quiet
                // AwaitingInput that strands the worker without a probe.
                let staged_outcome = self
                    .finalize_pr_transition(
                        execution_id,
                        staged_url.clone(),
                        WorkerPrCompletionTarget::InReview,
                        "stop_staged",
                    )
                    .await;
                if !matches!(staged_outcome, StopOutcome::AwaitingInput) {
                    return staged_outcome;
                }
                tracing::info!(
                    execution_id,
                    pr_url = %staged_url,
                    "stop event: staged-URL finalize refused by revision contribution gate; \
                     falling through to SHA-delta / no-op / nudge path (incident-004 AI-3)",
                );
            } else {
                return self
                    .finalize_pr_transition(
                        execution_id,
                        staged_url,
                        WorkerPrCompletionTarget::InReview,
                        "stop_staged",
                    )
                    .await;
            }
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

        // proposal_channel_error: a `boss propose` submission failed during
        // this execution's Stop-boundary window. File an attention and
        // count it — design §"Failure semantics": "the completion path
        // records proposal_channel_error on the run outcome and files an
        // engine-side attention, so the degradation is recorded, not
        // inferred from prose." Runs alongside the other two detection
        // passes above for the same reason: read-only against staged
        // in-memory state, never touches PR state, so it is safe ahead of
        // the running-status gate below.
        self.detect_and_file_proposal_channel_error(&execution).await;

        // Codex unobserved-command detection: a `command_execution` that
        // started with no observed completion before this turn boundary
        // (probe 6, exit-code investigation). Read-only against staged
        // in-memory state, never touches PR state, so — like the three
        // passes above — it is safe ahead of the running-status gate below.
        self.detect_and_file_unobserved_command_signal(&execution).await;

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
        //
        // The discard is recorded against each probe (`Dropped`, with this
        // reason) rather than performed silently: this path is where a
        // coordinator-issued probe accepted with a `next_turn_boundary`
        // commitment can legitimately be thrown away, so it is exactly the
        // path that must be able to explain itself afterwards.
        if let Some(signal) = self.unresolved_worker_signal_reason(&execution) {
            self.probe_queuer.clear_pending_probes(
                execution_id,
                &format!("discarded at a Stop that suppressed nudging: {signal}"),
            );
        }

        // AI #6 turn-loop gate (incident 001 §5): in Claude Code the
        // `Stop` hook fires after every assistant turn, not just at
        // worker exit. The cold-path fallback below (a jj revset plus a
        // GitHub walk) is only meaningful for a live worker that owns its
        // own turn loop; see [`super::worker_owns_turn_loop`] for why this
        // used to be spelled `status == waiting_human` and what it always
        // actually meant.
        //
        // Defense-in-depth on THIS path: both of the predicate's halves are
        // already enforced upstream in `on_stop_inner` — the non-live
        // statuses return `AlreadyTerminal`, and `pr_review` diverts to
        // `finalize_pr_review_pass` — so a hit here means one of those
        // funnels moved. It is load-bearing on the predicate's other caller,
        // `recheck_for_pr`, which the merge poller reaches by query rather
        // than by hook and which has no such funnel.
        //
        // Marker detection above already ran regardless of this gate, so a
        // `[blocked]` or `[deferred-scope]` signal is still filed and
        // visible to the coordinator even when this Stop parks here as a
        // no-op.
        if !super::worker_owns_turn_loop(&execution) {
            tracing::debug!(
                execution_id,
                status = %execution.status,
                kind = %execution.kind,
                "stop event: no staged URL and this execution does not own a live worker turn loop — skipping fallback",
            );
            return StopOutcome::RunningNoStagedPr;
        }

        // Neither the artifact nor the hook stream carried a URL, and the
        // worker is parked. Ask the driver whether its final message did — for
        // Claude, the "print the PR URL on its own line" convention. This runs
        // ahead of the cold-path `detect_pr` reconstruction (jj revset + a
        // GitHub API walk) because it is strictly cheaper and reads evidence
        // the worker itself produced.
        if self.stage_pr_url_from_driver_prose(&execution).await
            && let Some(recovered_url) = self
                .verified_staged_pr_url(execution_id, &execution, "stop event (driver fallback)")
                .await
        {
            tracing::info!(
                execution_id,
                pr_url = %recovered_url,
                "stop event: using PR URL recovered by the driver's final-message producer; skipping detector",
            );
            return self
                .finalize_pr_transition(
                    execution_id,
                    recovered_url,
                    WorkerPrCompletionTarget::InReview,
                    "stop_driver_fallback",
                )
                .await;
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
                    // Record that the baseline was rewritten, not just its
                    // new value: a later Stop's "head unchanged" finding
                    // against this absorbed baseline is a weaker claim than
                    // "unchanged since dispatch" and must not be trusted as
                    // `ContributionEvidence::ProvenAbsent` — see
                    // `execution_pr_head_baseline_absorbed`'s doc comment.
                    if let Err(err) = self.work_db.mark_execution_pr_head_baseline_absorbed(execution_id) {
                        tracing::warn!(
                            execution_id,
                            ?err,
                            "stop event: failed to stamp pr_head_baseline_absorbed after parent-push \
                             suppression; a later Stop may wrongly treat the rewritten baseline as \
                             proof of non-contribution",
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
                // `ProvenAbsent` carries THIS arm's finding — the bound
                // PR's head is byte-identical to the snapshot taken when
                // this run started — into that gate, where it is
                // load-bearing on the finalize decision. Without it, the
                // gate's "open + mergeable + CI clean" predicate is
                // trivially true at t=0 for every revision (that is the
                // state a reviewer pass dispatches one into) and any Stop
                // boundary terminalizes the run as delivered. See
                // `health_alone_satisfies_deliverable`.
                //
                // "Byte-identical to the snapshot taken when this run
                // started" is only true when `pr_head_before` has never
                // been rewritten. The parent-push suppression path earlier
                // in this function absorbs a head movement it attributes to
                // the concurrently-active parent worker by overwriting
                // `pr_head_before` with the new head — so on a LATER Stop,
                // "head unchanged" can mean "unchanged since that absorbed
                // baseline", not "unchanged since dispatch". If this
                // execution's baseline was ever absorbed, downgrade the
                // finding to `Indeterminate`: "we could not measure" is the
                // honest claim once the reference point is no longer the
                // dispatch-time head, and refusing here would strand a
                // revision whose own push evidence was missed and absorbed
                // into the baseline, with no way to ever satisfy the gate
                // (mono#2606 revision).
                let contribution_evidence = match self.work_db.execution_pr_head_baseline_absorbed(execution_id) {
                    Ok(true) => ContributionEvidence::Indeterminate,
                    Ok(false) => ContributionEvidence::ProvenAbsent,
                    Err(err) => {
                        tracing::warn!(
                            execution_id,
                            ?err,
                            "stop event: failed to read pr_head_baseline_absorbed; treating as \
                             absorbed (Indeterminate) to avoid a false ProvenAbsent refusal",
                        );
                        ContributionEvidence::Indeterminate
                    }
                };
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
                    .try_finalize_satisfied_deliverable_on_stop(
                        execution_id,
                        &execution,
                        &pr_url,
                        contribution_evidence,
                    )
                    .await
                {
                    return outcome;
                }
                // Sanctioned no-op terminal for a revision (the honest exit
                // the gate above deliberately no longer manufactures). A
                // revision that pushed nothing and emitted NO_CHANGES_NEEDED
                // is making an explicit, checkable claim — "the finding I
                // was dispatched for needs no code change" — rather than
                // having that conclusion inferred for it from a PR whose
                // health predates the run. Placed AFTER the conflict
                // refusal above so the claim can never launder a conflict
                // GitHub still reports, and after the satisfied gate so the
                // stronger evidence (merged / queued / conflict cleared)
                // wins where it applies.
                if execution.kind == ExecutionKind::RevisionImplementation
                    && self.worker_signalled_no_op(execution_id).await
                {
                    // Same refusal the primary-implementation no-op gate
                    // applies, for the same reason: an unobserved command
                    // is exactly what undermines a "I checked, nothing is
                    // needed" claim — Boss never saw whether the check ran.
                    // `consume_unresolved` (not `list`) so one abandoned
                    // command from turns ago cannot refuse every later
                    // claim for the rest of a long multi-turn run.
                    if self.staged_unobserved_commands.consume_unresolved(execution_id) {
                        tracing::warn!(
                            execution_id,
                            bound_pr_url = %pr_url,
                            "stop event: revision emitted NO_CHANGES_NEEDED but this run left a \
                             command_execution unobserved since the gate last checked — refusing \
                             the no-op claim; falling through to the nudge instead",
                        );
                    } else {
                        tracing::info!(
                            execution_id,
                            bound_pr_url = %pr_url,
                            "stop event: revision pushed nothing and declared NO_CHANGES_NEEDED — \
                             closing it as a declared no-op (no PR, no nudge) and filing an \
                             attention item so the unaddressed finding is visible",
                        );
                        self.file_revision_no_op_attention(&execution, &pr_url).await;
                        return self.finalize_no_op_completion(&execution).await;
                    }
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
                    // `Indeterminate`, not `ProvenAbsent`: this arm is
                    // reached precisely because the SHA comparison could
                    // not be made (no baseline, or the head fetch failed).
                    // "We could not measure" is a different claim from "we
                    // measured, and it did not move" — the gate still
                    // accepts PR health here, and logs loudly that it did,
                    // because refusing would strand any revision whose
                    // dispatch-time snapshot failed in a state no Stop can
                    // ever finalize (the stuck-revision dead end).
                    if let Some(outcome) = self
                        .try_finalize_satisfied_deliverable_on_stop(
                            execution_id,
                            &execution,
                            &bound_pr_url,
                            ContributionEvidence::Indeterminate,
                        )
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
                // ALREADY DONE. The marker is a self-report: alongside the real
                // Stop boundary (`waiting_human`), no PR on this branch
                // (PrStatus::None), and none bound to the chore (the
                // resolve_bound_pr_url branch above returned), we require positive
                // `workspace_diff_verifier` evidence that `jj diff --from
                // main@origin --to @ --summary` is empty. Negative or unavailable
                // evidence refuses the claim. If all checks pass, the sanctioned
                // NO_CHANGES_NEEDED marker is a SUCCESS, not a failure to be
                // nudged: close the task as done without a PR.
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
                    // "validation passed / nothing to do" is exactly the
                    // claim an unobserved command undermines: Boss never saw
                    // whether that command actually succeeded, so it cannot
                    // trust the worker's verification. Refuse the no-op
                    // claim and fall through to the normal produce-a-PR
                    // nudge rather than closing the task as done.
                    //
                    // `consume_unresolved` (not `list`) is the
                    // right read here: it answers "has a command gone
                    // unobserved since the last time this gate fired?", not
                    // "has this run ever left a command unobserved?" — see
                    // `codex_unobserved_command::UnobservedCommandTracker`'s
                    // type doc for why the latter question, in a long-lived
                    // multi-turn Codex session that reaches this Stop many
                    // times, would refuse every later no-op claim for the
                    // rest of the run over one abandoned command from turns
                    // ago. This read also clears the flag, so a clean turn
                    // that follows gets a fair NO_CHANGES_NEEDED evaluation.
                    if self.staged_unobserved_commands.consume_unresolved(execution_id) {
                        tracing::warn!(
                            execution_id,
                            expected_branch = %expected_branch,
                            kind = %execution.kind,
                            "stop event: worker emitted NO_CHANGES_NEEDED but this run left at least \
                             one Codex command_execution unobserved since the gate last checked \
                             (item.started with no item.completed) — refusing the no-op claim; \
                             falling through to the produce-a-PR nudge instead",
                        );
                    } else if let Some(workspace_path) = execution.workspace_path.as_deref() {
                        match self
                            .workspace_diff_verifier
                            .is_workspace_contribution_empty(std::path::Path::new(workspace_path))
                            .await
                        {
                            Ok(true) => {
                                tracing::info!(
                                    execution_id,
                                    expected_branch = %expected_branch,
                                    kind = %execution.kind,
                                    "stop event: worker emitted NO_CHANGES_NEEDED with no workspace contribution and no PR produced — work already done; closing task as a no-op (no PR, no nudge)"
                                );
                                return self.finalize_no_op_completion(&execution).await;
                            }
                            Ok(false) => tracing::warn!(
                                execution_id,
                                expected_branch = %expected_branch,
                                kind = %execution.kind,
                                "stop event: worker emitted NO_CHANGES_NEEDED but the workspace has a contribution — refusing the no-op claim; falling through to the produce-a-PR nudge instead",
                            ),
                            Err(err) => tracing::error!(
                                execution_id,
                                expected_branch = %expected_branch,
                                kind = %execution.kind,
                                ?err,
                                "stop event: could not verify whether the workspace has a contribution — refusing the no-op claim; falling through to the produce-a-PR nudge instead",
                            ),
                        }
                    } else {
                        tracing::error!(
                            execution_id,
                            expected_branch = %expected_branch,
                            kind = %execution.kind,
                            "stop event: worker emitted NO_CHANGES_NEEDED without a recorded workspace path — refusing the no-op claim; falling through to the produce-a-PR nudge instead",
                        );
                    }
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
