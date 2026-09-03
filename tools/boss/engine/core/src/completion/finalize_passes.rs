//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

/// Result of [`WorkerCompletionHandler::check_pure_rebase_skip`].
/// `post_head` carries the PR's current head OID whenever the gate got as
/// far as fetching it, regardless of whether the gate ultimately skipped —
/// so the caller can pass it to [`WorkerCompletionHandler::check_noop_skip`]
/// and avoid a second, identical `fetch_pr_head_oid` round trip for the
/// same PR.
pub(super) struct PureRebaseGateOutcome {
    pub(super) skip_reason: Option<&'static str>,
    pub(super) post_head: Option<String>,
}

impl PureRebaseGateOutcome {
    fn no_skip() -> Self {
        Self {
            skip_reason: None,
            post_head: None,
        }
    }

    fn no_skip_with_head(post_head: String) -> Self {
        Self {
            skip_reason: None,
            post_head: Some(post_head),
        }
    }
}

/// `(outcome, produced_task_id, detail)` — the triple both
/// [`WorkerCompletionHandler::automation_outcome_from_proposal`] and
/// [`WorkerCompletionHandler::legacy_automation_triage_decision`] produce for
/// [`WorkerCompletionHandler::finalize_automation_triage`] to record.
type AutomationTriageDecision = (&'static str, Option<String>, Option<String>);

impl WorkerCompletionHandler {
    /// Common Fresh/Merged transition path shared by `on_stop_inner`
    /// and `recheck_for_pr`. Records the completion, releases the
    /// cube lease + pane, publishes invalidation events, and returns
    /// the matching [`StopOutcome`]. `source` distinguishes call
    /// sites in the publish reason and tracing — `"stop"` for the
    /// Maint task 6: resolve a finished `automation_triage` execution via the
    /// marker protocol and finalise both its `automation_runs` row and the
    /// execution itself.
    ///
    /// The worker was told to write its decision to the engine-owned
    /// structured-output artifact and to end its final message with the
    /// matching `automation: task <id>` / `automation: skip — <reason>`
    /// marker. Steps:
    /// 1. resolve the decision — artifact first, the driver's marker-line
    ///    producer as fallback (see
    ///    [`crate::automation_triage::resolve_triage_decision`]);
    /// 2. for a `task` decision, verify the id resolves to a task carrying
    ///    this automation's provenance — so a misbehaving agent can't pass off
    ///    an unrelated task as its own output;
    /// 3. record the terminal outcome (`produced_task` / `skipped`, or keep
    ///    `failed_will_retry` for a missing / ambiguous / unverifiable
    ///    decision);
    /// 4. finalise the execution (`completed`) and release pane + workspace.
    ///
    /// Design implementation task 11
    /// (`worker-proposal-api-replace-fragile-worker-to-engine-seams.md` —
    /// the automation-triage-outcome seam migration, the worst-failing seam
    /// in the design's inventory: a measurement of 67 consecutive
    /// `produced_task` finalizations found none decided by a valid marker):
    /// when `automation_outcome_proposals_seam` is on (composed with the
    /// `worker_proposals` master flag), this reads the execution's
    /// `automation_outcome` worker-proposal row FIRST, via
    /// [`Self::automation_outcome_from_proposal`] — task 6's applier
    /// (`crate::work::proposal_apply::apply_automation_outcome`) already
    /// decided it synchronously and provenance-checked at submission time, so
    /// this never re-derives or re-guesses the decision, and a
    /// `produced_task` proposal rejected for a provenance mismatch finalizes
    /// `failed_will_retry` via that rejection rather than falling through to
    /// the open-task guess below. Only when no `automation_outcome` proposal
    /// exists does the legacy marker-parse + recovery-heuristic chain below
    /// still run, counted via `AUTOMATION_OUTCOME_FALLBACK_HIT` and
    /// WARN-logged — this seam's explicit exit criterion. Flag off
    /// reproduces the legacy-only behavior exactly.
    pub(super) async fn finalize_automation_triage(&self, execution: &crate::work::WorkExecution) -> StopOutcome {
        // Same collection contract as the review pass: only positive proof
        // the artifact is untrustworthy terminalizes; a remote triage
        // worker whose decision artifact could not be pulled otherwise
        // falls through to `resolve_triage_decision`'s driver-fallback /
        // no-decision recovery below exactly as a genuinely absent artifact
        // already does.
        if let RemoteCollectionResult::Failed { host_id, reason } = self
            .collect_remote_structured_output(
                execution,
                crate::structured_output::StructuredOutputKind::TriageDecision,
            )
            .await
        {
            tracing::error!(execution_id = %execution.id, host_id = %host_id, error = %reason, "automation triage finalize: cannot safely continue after collection failure");
            return self
                .finalize_remote_collection_failure(execution, &host_id, &reason)
                .await;
        }
        let automation_id = execution.work_item_id.clone();

        let automation_outcome_proposals_first = self.feature_flags.is_enabled("worker_proposals")
            && self.feature_flags.is_enabled("automation_outcome_proposals_seam");
        let (from_proposal, proposal_row_existed) = if automation_outcome_proposals_first {
            self.automation_outcome_from_proposal(&execution.id)
        } else {
            (None, false)
        };
        let used_proposal = from_proposal.is_some();

        let (outcome, produced_task_id, detail): (&str, Option<String>, Option<String>) = match from_proposal {
            Some(resolved) => resolved,
            None => self.legacy_automation_triage_decision(execution, &automation_id).await,
        };

        match self.work_db.finalize_automation_triage_run(
            &execution.id,
            outcome,
            produced_task_id.as_deref(),
            detail.as_deref(),
        ) {
            Ok(true) => {
                if automation_outcome_proposals_first && !used_proposal {
                    self.record_automation_outcome_fallback_hit(
                        execution,
                        detail.as_deref().unwrap_or(""),
                        proposal_row_existed,
                    );
                }
            }
            Ok(false) => tracing::warn!(
                execution_id = %execution.id,
                automation_id = %automation_id,
                "no automation_runs row matched this triage execution; outcome not recorded",
            ),
            Err(err) => tracing::error!(
                execution_id = %execution.id,
                ?err,
                "failed to finalise automation_runs row for triage execution",
            ),
        }

        // The decision has been consumed and recorded; reap the artifact so a
        // re-fired automation reusing this execution id can never read a stale
        // one.
        crate::structured_output::clear_all(&self.structured_output_dir, &execution.id);

        // Finalise the execution + release pane and cube workspace, mirroring
        // the PR-completion finalizer's release order. Capture the lease id
        // before `complete_pane_parked_execution` nulls the lease columns.
        //
        // This unconditionally drives the execution to `completed` — it does
        // NOT depend on there being a still-`active` work_runs row, because
        // `PaneSpawnRunner` already closed that row out at spawn-confirm time
        // (see `complete_pane_parked_execution`'s doc). Looping over
        // `active_run_ids_for_execution` here (as this used to) found nothing
        // in the common single-turn case, silently leaving the execution
        // stuck `waiting_human` — which is exactly what let the pane-death
        // sweep re-finalize an already-finalized triage run later with a
        // misleading pane-died detail.
        let lease_id = execution.cube_lease_id.clone();
        let workspace_path = execution.workspace_path.clone();
        // Marked before the terminalizing write — see `super::teardown`.
        // `AutomationTriage` is one of the two kinds `terminal_work_sweep`
        // reaps on execution terminality alone (it can never resolve their
        // work-item ids), so this path is if anything MORE exposed to the
        // race than the PR-completion one.
        let teardown = self.begin_teardown(&execution.id);
        match self.work_db.complete_pane_parked_execution(
            &execution.id,
            "completed",
            Some(&format!("automation triage: {outcome}")),
        ) {
            Ok(Some(_)) => {}
            Ok(None) => tracing::debug!(
                execution_id = %execution.id,
                "automation triage finalise: execution already terminal; nothing to do",
            ),
            Err(err) => tracing::error!(
                execution_id = %execution.id,
                ?err,
                "failed to finalise triage execution row",
            ),
        }
        self.finish_worker_teardown(
            &execution.id,
            lease_id.as_deref(),
            workspace_path.as_deref().map(std::path::Path::new),
            "automation_triage",
            teardown,
        )
        .await;
        self.publisher
            .publish(
                &execution.id,
                &automation_id,
                "completed",
                "automation_triage_completed",
            )
            .await;

        tracing::info!(
            execution_id = %execution.id,
            automation_id = %automation_id,
            outcome,
            produced_task_id = ?produced_task_id,
            detail = ?detail,
            "automation triage finalised",
        );
        StopOutcome::AutomationTriage {
            outcome: outcome.to_owned(),
        }
    }

    /// Read `execution`'s `automation_outcome` worker-proposal row and
    /// translate its already-decided disposition into the
    /// `(outcome, produced_task_id, detail)` triple
    /// [`Self::finalize_automation_triage`] records — see that method's doc
    /// for the design context. Returns `(None, row_existed)` when no operative
    /// `automation_outcome` proposal is available (the caller then runs the
    /// legacy marker-parse + recovery-heuristic chain as a counted fallback).
    /// `row_existed` distinguishes why: `false` when no `automation_outcome`
    /// proposal row exists for this execution at all (or the DB lookup itself
    /// errored — fail open to the legacy path rather than silently dropping
    /// the finalization), `true` when a row exists but was left in an
    /// unexpected state (`Proposed`/`Superseded`/`Expired`, each already
    /// WARN-logged below) — [`Self::record_automation_outcome_fallback_hit`]
    /// uses this so its own WARN doesn't contradict the more specific one
    /// just logged.
    ///
    /// [`crate::work::WorkDb::list_worker_proposals_for_execution`] orders by
    /// `created_at DESC`, and task 6's applier
    /// (`crate::work::proposal_apply::apply_automation_outcome`) marks every
    /// PRIOR undecided/applied `automation_outcome` proposal `superseded` the
    /// moment a new one applies — so the newest row is always the operative
    /// one; it is never itself marked `superseded` (only older rows are, as a
    /// side effect of a later submission). A triage worker that revises its
    /// outcome mid-run (submits `boss propose automation-outcome` a second
    /// time) is therefore handled for free: the newest row wins, with no
    /// special-casing here.
    fn automation_outcome_from_proposal(&self, execution_id: &str) -> (Option<AutomationTriageDecision>, bool) {
        let proposals = match self
            .work_db
            .list_worker_proposals_for_execution(execution_id, ProposalKind::AutomationOutcome)
        {
            Ok(proposals) => proposals,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "automation_outcome_proposals_seam: failed to look up proposals for this \
                     execution; falling back to the legacy marker parser",
                );
                return (None, false);
            }
        };
        let Some(latest) = proposals.first() else {
            return (None, false);
        };

        let resolved = match latest.state {
            ProposalState::Rejected => Some((
                AUTOMATION_OUTCOME_FAILED_WILL_RETRY,
                None,
                Some(format!(
                    "automation_outcome proposal {} rejected: {}",
                    latest.id,
                    latest.decision_reason.as_deref().unwrap_or("no reason recorded"),
                )),
            )),
            ProposalState::Applied => match latest.applied_ref.clone() {
                Some(task_id) => Some((
                    AUTOMATION_OUTCOME_PRODUCED_TASK,
                    Some(task_id.clone()),
                    Some(format!(
                        "produced task {task_id} (via automation-outcome proposal {})",
                        latest.id
                    )),
                )),
                None => {
                    let reason = serde_json::from_str::<serde_json::Value>(&latest.payload_json)
                        .ok()
                        .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(str::to_owned))
                        .unwrap_or_else(|| "no reason given".to_owned());
                    Some((
                        AUTOMATION_OUTCOME_SKIPPED,
                        None,
                        Some(format!("{reason} (via automation-outcome proposal {})", latest.id)),
                    ))
                }
            },
            // `AutomationOutcome` auto-applies synchronously at submission
            // time (see `crate::work::proposal_apply::apply_policy`), so the
            // newest row should never be `Proposed`/`Superseded` — but fail
            // safe rather than guess: treat as "no operative proposal" so the
            // legacy path still runs as a counted fallback. The row DOES
            // exist here, so the fallback-hit WARN must say so.
            ProposalState::Proposed | ProposalState::Superseded => {
                tracing::warn!(
                    execution_id,
                    proposal_id = %latest.id,
                    state = %latest.state,
                    "automation_outcome_proposals_seam: newest automation_outcome proposal is \
                     unexpectedly not Applied/Rejected; falling back to the legacy marker parser",
                );
                return (None, true);
            }
            ProposalState::Expired => {
                tracing::warn!(
                    execution_id,
                    proposal_id = %latest.id,
                    "automation_outcome_proposals_seam: newest automation_outcome proposal is \
                     unexpectedly Expired (not an in-flight-only kind); falling back to the \
                     legacy marker parser",
                );
                return (None, true);
            }
        };

        (resolved, true)
    }

    /// The pre-migration marker-parse + recovery-heuristic chain, unchanged
    /// in behavior: the worker was told to end its final message with
    /// exactly one of `automation: task <id>` or `automation: skip — <reason>`.
    /// Steps:
    /// 1. read the final assistant message and parse the decision;
    /// 2. for a `task` marker, verify the id resolves to a task carrying this
    ///    automation's provenance — so a misbehaving agent can't pass off an
    ///    unrelated task as its own output;
    /// 3. record the terminal outcome (`produced_task` / `skipped`, or keep
    ///    `failed_will_retry` for a missing / ambiguous / unverifiable marker).
    ///
    /// Called by [`Self::finalize_automation_triage`] either unconditionally
    /// (seam off) or as the counted fallback (seam on, no `automation_outcome`
    /// proposal found).
    async fn legacy_automation_triage_decision(
        &self,
        execution: &crate::work::WorkExecution,
        automation_id: &str,
    ) -> AutomationTriageDecision {
        // The transcript state is still read (and kept) because the
        // no-decision detail below distinguishes "ran but reported nothing"
        // from "produced no transcript at all" — but it is only the *fallback*
        // channel for the decision itself.
        let (driver, transcript) = self.read_final_triage_message_with_driver(&execution.id).await;
        let final_message = match &transcript {
            TriageTranscript::FinalMessage(text) => Some(text.as_str()),
            TriageTranscript::NoPath | TriageTranscript::Unreadable | TriageTranscript::NoAssistantText { .. } => None,
        };
        let decision = crate::automation_triage::resolve_triage_decision(
            crate::driver_transcript::driver_or_default(driver.as_deref()),
            &self.structured_output_dir,
            &execution.id,
            final_message,
        );

        match &decision {
            TriageDecision::ProducedTask(marker_id) => {
                match self.work_db.get_work_item_resolving_short_id(marker_id) {
                    Ok(Some(WorkItem::Task(t))) | Ok(Some(WorkItem::Chore(t)))
                        if t.source_automation_id.as_deref() == Some(automation_id) =>
                    {
                        // Explicit success detail (not `None`): it overwrites
                        // the pessimistic dispatch-time placeholder so a row
                        // that still reads "dispatched; awaiting …" can only
                        // mean the worker never reached Stop (crashed/hung).
                        (
                            AUTOMATION_OUTCOME_PRODUCED_TASK,
                            Some(t.id.clone()),
                            Some(format!("produced task {}", t.short_label())),
                        )
                    }
                    other => {
                        tracing::warn!(
                            execution_id = %execution.id,
                            automation_id = %automation_id,
                            marker_id,
                            resolved_some = ?other.as_ref().map(|o| o.is_some()),
                            "triage emitted a task marker but no task with this automation's \
                             provenance matched; leaving run failed_will_retry",
                        );
                        (
                            AUTOMATION_OUTCOME_FAILED_WILL_RETRY,
                            None,
                            Some(format!(
                                "triage emitted `automation: task {marker_id}` but no task \
                                     with this automation's provenance was found"
                            )),
                        )
                    }
                }
            }
            TriageDecision::Skip(reason) => {
                let reason = if reason.is_empty() {
                    "no reason given".to_owned()
                } else {
                    reason.clone()
                };
                (AUTOMATION_OUTCOME_SKIPPED, None, Some(reason))
            }
            TriageDecision::NoDecision => {
                // Build the base detail for the no-marker case (used when
                // recovery fails or is inapplicable).
                let base_detail = triage_no_decision_detail(&transcript);
                // Primary decision-derivation path: a valid marker is the
                // exception, not the rule (a measurement of 67 recent
                // `produced_task` finalizations found none with one), so this
                // is not a rare "recovery" — it is how almost every triage run
                // is actually decided. Derive the outcome from what the run
                // provably did: if it called `boss task create --automation`,
                // the open-task record is ground truth for that; if it didn't,
                // but its own final words plainly concluded there is nothing
                // to do, that conclusion stands in for the missing marker.
                // Bounding the task lookup to tasks created no earlier than
                // this execution started (`not_before_epoch`) keeps a task
                // left open by an *earlier* run (e.g. stuck in review) from
                // being misattributed to this one.
                //
                // This matters most on retry: without it, a run that created a
                // task but lost its marker records `failed_will_retry`, and the
                // next fire creates a second task for the same work. (Reaching
                // the open-task cap itself is prevented upstream by the
                // scheduler's `suppressed_at_limit` gate, not here — the
                // `not_before_epoch` bound means rows from earlier runs are
                // deliberately invisible to this lookup.)
                match self
                    .work_db
                    .find_most_recent_open_task_for_automation(automation_id, execution.created_epoch())
                {
                    Ok(Some(task)) => {
                        tracing::info!(
                            execution_id = %execution.id,
                            automation_id = %automation_id,
                            recovered_task_id = %task.id,
                            base_detail,
                            "triage run ended without a valid decision marker but \
                             found an open task produced by this run; recording as \
                             produced_task (marker-recovery is the primary decision \
                             path — see find_most_recent_open_task_for_automation)",
                        );
                        (
                            AUTOMATION_OUTCOME_PRODUCED_TASK,
                            Some(task.id),
                            Some(format!(
                                "produced_task (marker-recovery): task was created \
                                 but decision marker was missing — {base_detail}"
                            )),
                        )
                    }
                    Ok(None) => match recover_skip_reason(&decision, &transcript) {
                        // No task was created AND the worker's final message
                        // plainly concluded there is nothing to do (a clean-repo
                        // / no-warnings verdict) but it botched the exact skip
                        // marker. Record `skipped` — symmetric with the
                        // produced-task marker-recovery above. Without this, a
                        // run that correctly found nothing loops
                        // `failed_will_retry` forever, re-running a full session
                        // to re-prove an already-clean repo.
                        Some(reason) => {
                            tracing::info!(
                                execution_id = %execution.id,
                                automation_id = %automation_id,
                                base_detail,
                                "triage run created no task and emitted no skip marker, but its \
                                 final message plainly concluded there is nothing to do; recording \
                                 as skipped (skip marker-recovery is the primary decision path)",
                            );
                            (
                                AUTOMATION_OUTCOME_SKIPPED,
                                None,
                                Some(format!(
                                    "skipped (marker-recovery): worker concluded no work but \
                                     emitted no skip marker — {reason}"
                                )),
                            )
                        }
                        // Neither a task nor a recoverable skip conclusion: a
                        // genuine failure, distinct from the two paths above —
                        // this is the only case that should still read as
                        // `failed_will_retry` / "Failed (retrying)" to an
                        // operator.
                        None => {
                            tracing::warn!(
                                execution_id = %execution.id,
                                automation_id = %automation_id,
                                base_detail,
                                "triage run ended with no valid decision marker, no task \
                                 created, and no recoverable skip conclusion; recording as \
                                 failed_will_retry (genuine failure, not marker-recovery)",
                            );
                            (
                                AUTOMATION_OUTCOME_FAILED_WILL_RETRY,
                                None,
                                Some(format!(
                                    "no decision: no task created and no no-work \
                                     conclusion — {base_detail}"
                                )),
                            )
                        }
                    },
                    Err(err) => {
                        tracing::warn!(
                            execution_id = %execution.id,
                            automation_id = %automation_id,
                            ?err,
                            "triage recovery: DB query for open tasks failed; \
                             recording as failed_will_retry",
                        );
                        (
                            AUTOMATION_OUTCOME_FAILED_WILL_RETRY,
                            None,
                            Some(format!("db error during recovery lookup — {base_detail}")),
                        )
                    }
                }
            }
        }
    }

    /// P3b: resolve a finished `answer_agent` execution when its Stop hook
    /// fires. Unlike triage, there is no marker protocol to parse here — the
    /// agent's *only* permitted write is `CommentsPostAnswer` (`boss comment
    /// reply`), which the RPC handler already used to complete the
    /// `answer_agent_runs` row, post the `entry_kind = 'answer'` thread
    /// entry, and transition the comment `answering → answered` mid-session.
    /// So this handler's real job is the failure path: if the run is STILL
    /// `running` when Stop fires (the agent crashed, ran out of turns, or
    /// otherwise ended without ever posting a reply), resolve it here so the
    /// comment doesn't sit `answering` forever — mark the run `failed` and
    /// post an apology thread entry standing in for the missing answer,
    /// mirroring the design's `answering → answered` transition (an
    /// unanswered question is still "no longer in flight").
    ///
    /// Either way, finalise the execution (`completed`) and release its pane
    /// + workspace, mirroring `finalize_automation_triage`'s tail.
    pub(super) async fn finalize_answer_agent(&self, execution: &crate::work::WorkExecution) -> StopOutcome {
        let comment_id = execution.work_item_id.clone();
        let replied = match self.work_db.running_answer_agent_run_for_comment(&comment_id) {
            Ok(Some(run)) => {
                match self
                    .work_db
                    .recover_unanswered_comment(&comment_id, Some(&run.id), "no_reply_posted")
                {
                    Ok(_) => crate::answer_agent_observability::record_failed(&self.metrics, "no_reply_posted"),
                    Err(err) => tracing::warn!(
                        execution_id = %execution.id,
                        run_id = %run.id,
                        ?err,
                        "answer-agent finalizer: failed to recover the comment from its unanswered run",
                    ),
                }
                false
            }
            Ok(None) => true, // already completed via `CommentsPostAnswer` mid-session
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    comment_id = %comment_id,
                    ?err,
                    "answer-agent finalizer: failed to look up the running run; \
                     leaving comment state as-is",
                );
                true
            }
        };

        // Finalise the execution + release pane and cube workspace, mirroring
        // the triage finalizer's release order and its use of
        // `complete_pane_parked_execution` (see that finalizer's comment for
        // why this does not depend on there being a still-`active` run).
        let lease_id = execution.cube_lease_id.clone();
        let workspace_path = execution.workspace_path.clone();
        // Marked before the terminalizing write — see `super::teardown`.
        // `AnswerAgent` is the other kind `terminal_work_sweep` reaps on
        // execution terminality alone.
        let teardown = self.begin_teardown(&execution.id);
        match self.work_db.complete_pane_parked_execution(
            &execution.id,
            "completed",
            Some(if replied {
                "answer agent: replied"
            } else {
                "answer agent: no reply posted"
            }),
        ) {
            Ok(Some(_)) => {}
            Ok(None) => tracing::debug!(
                execution_id = %execution.id,
                "answer-agent finalise: execution already terminal; nothing to do",
            ),
            Err(err) => tracing::error!(
                execution_id = %execution.id,
                ?err,
                "failed to finalise answer-agent execution row",
            ),
        }
        self.finish_worker_teardown(
            &execution.id,
            lease_id.as_deref(),
            workspace_path.as_deref().map(std::path::Path::new),
            "answer_agent",
            teardown,
        )
        .await;
        self.publisher
            .publish(&execution.id, &comment_id, "completed", "answer_agent_completed")
            .await;

        tracing::info!(
            execution_id = %execution.id,
            comment_id = %comment_id,
            replied,
            "answer-agent execution finalised",
        );
        StopOutcome::AnswerAgent { replied }
    }

    /// Run [`Self::finalize_answer_agent`] for `execution_id` from outside the
    /// Stop path — the entry point [`crate::answer_agent_completion_sweep`]
    /// uses once it has positive evidence the agent's work is done but no turn
    /// boundary ever arrived to say so.
    ///
    /// Deliberately the SAME finalizer the Stop path runs, not a parallel
    /// teardown: it is what keeps the execution row and the pool slot moving
    /// together (`complete_pane_parked_execution` then
    /// `finish_worker_teardown`), so there is no window where one says finished
    /// and the other says claimed.
    ///
    /// Returns `None` — having done nothing — when the execution is unknown,
    /// is not an `answer_agent`, or is already terminal. Those are the races
    /// the caller must not act on, not conditions to force through.
    pub async fn finalize_answer_agent_execution(&self, execution_id: &str) -> Option<StopOutcome> {
        let execution = self.work_db.get_execution(execution_id).ok()?;
        if execution.kind != ExecutionKind::AnswerAgent || execution.status.is_terminal() {
            return None;
        }
        Some(self.finalize_answer_agent(&execution).await)
    }

    /// Finalise a `pr_review` reviewer execution when its Stop
    /// hook fires. The reviewer never opens a PR; instead, it reads the
    /// producing task's PR diff and emits structured `ReviewResult` JSON in
    /// a fenced code block in its final message. This handler:
    ///
    /// 1. Reads the reviewer's final assistant message from its transcript.
    /// 2. Extracts and parses the `ReviewResult` JSON block.
    /// 3. Applies the engine severity gate (design §3): any `critical`/`high`
    ///    finding, or any `regression` finding (regardless of severity), warrants
    ///    a revision. `revision_warranted = false` alone does not suppress the gate.
    ///    4a. If the gate passes: creates a revision task on the producing task
    ///    with the rendered findings as `revision_instructions`, `source =
    ///    pr_review`, dispatched on the general worker pool (`autostart = true`).
    ///    The producing task advances from `active` → `in_review` at this point;
    ///    the revision is an additional follow-up child task.
    ///    4b. If the gate does not pass (no qualifying findings, or no parseable
    ///    `ReviewResult`): the producing task advances to `in_review`.
    ///
    /// Until this handler fires, the producing task is held in `active` (Doing)
    /// with `pr_url` stamped and `ai_reviewing = true` in the derived work-tree
    /// projection. A fallback sweep in the merge poller ensures the hold always
    /// resolves even if this Stop never arrives.
    ///
    /// In either case the reviewer execution is completed and its workspace
    /// released — it is always terminal after this handler runs.
    pub(super) async fn finalize_pr_review_pass(&self, execution: &crate::work::WorkExecution) -> StopOutcome {
        // A persisted batch member uses proposal delivery exclusively. Do
        // this before reading any artifact or transcript so a batch leaf can
        // never fall through to the legacy single-reviewer materializer.
        match self.work_db.review_batch_member_for_execution(&execution.id) {
            Ok(Some(member)) => return self.finalize_review_batch_member(execution, member).await,
            Ok(None) => {}
            Err(err) => {
                tracing::error!(
                    execution_id = %execution.id,
                    ?err,
                    "pr_review finalize: could not determine batch membership",
                );
                return StopOutcome::DbError;
            }
        }

        // Captured before `record_worker_pr_completion` below nulls
        // `workspace_path` in the same transaction that terminalizes this
        // execution — this path terminalizes a parked-live execution, so it
        // owns driver teardown.
        let workspace_path = execution.workspace_path.clone();
        let producing_task_id = &execution.work_item_id;

        // Trace marker distinguishing a re-review triggered by a revision's
        // push from the original first-push review (2026-07-01 revision-
        // review experiment) — lets the engine surfaces count cycles and
        // time spent per trigger kind without a schema change.
        let trigger = match self.work_db.get_work_item(producing_task_id) {
            Ok(WorkItem::Task(ref t)) if t.kind == TaskKind::Revision => "revision_push",
            _ => "primary_push",
        };

        // Look up the producing task to retrieve its pr_url (stamped during
        // the PendingReview write when the reviewer was enqueued).
        let pr_url = match self.work_db.get_work_item(producing_task_id) {
            Ok(WorkItem::Task(ref t)) | Ok(WorkItem::Chore(ref t)) => match t.pr_url.as_deref() {
                Some(url) if !url.is_empty() => url.to_owned(),
                _ => {
                    tracing::warn!(
                        execution_id = %execution.id,
                        producing_task_id,
                        "pr_review finalize: producing task has no pr_url; \
                         cannot advance to in_review",
                    );
                    return StopOutcome::DbError;
                }
            },
            Ok(other) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    producing_task_id,
                    item_type = ?other,
                    "pr_review finalize: work_item_id does not resolve to a task/chore",
                );
                return StopOutcome::DbError;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    producing_task_id,
                    ?err,
                    "pr_review finalize: could not load producing task",
                );
                return StopOutcome::DbError;
            }
        };

        // Read the reviewer's ReviewResult. PRIMARY channel: the engine-owned
        // structured-output artifact the reviewer wrote, schema-validated here
        // via `ReviewResult::from_json`. TRANSITIONAL FALLBACK: ask the Claude
        // driver's fallback producer to recover the JSON from the transcript's
        // final message (fenced / bare) — this covers remote workers, whose
        // artifact is written on the remote host and not readable here, and
        // any local artifact-write failure.
        //
        // `parse_error` captures the serde error from the artifact if it was
        // present-but-invalid, else from the driver fallback's most-preferred
        // failing candidate, so the reviewer re-prompt names the specific
        // field + type mismatch rather than a generic "write valid JSON"
        // instruction.
        let mut parse_error: Option<String> = None;

        // A remote wrapper writes its artifact on the remote host. Collection
        // lives at the read site, not the Stop dispatcher, so rechecks and
        // recovery finalization paths observe the same artifact. Only a
        // `Failed` outcome — positive proof the artifact is untrustworthy —
        // terminalizes; every other outcome falls through to the ordinary
        // artifact/transcript read below.
        if let RemoteCollectionResult::Failed { host_id, reason } = self
            .collect_remote_structured_output(execution, crate::structured_output::StructuredOutputKind::ReviewResult)
            .await
        {
            tracing::error!(execution_id = %execution.id, host_id = %host_id, error = %reason, "pr_review finalize: cannot safely continue after collection failure");
            return self
                .finalize_remote_collection_failure(execution, &host_id, &reason)
                .await;
        }

        let from_artifact = match crate::structured_output::read(
            &self.structured_output_dir,
            &execution.id,
            crate::structured_output::StructuredOutputKind::ReviewResult,
        ) {
            None => None,
            Some(raw) => match crate::pr_review::ReviewResult::from_json(&raw) {
                Ok(result) => Some(result),
                Err(err) => {
                    let err_str = err.to_string();
                    tracing::warn!(
                        execution_id = %execution.id,
                        producing_task_id,
                        error = %err_str,
                        "pr_review finalize: structured-output artifact present but did not \
                         validate as ReviewResult; trying the driver's transcript fallback",
                    );
                    parse_error = Some(err_str);
                    None
                }
            },
        };
        let review_result = match from_artifact {
            Some(result) => Some(result),
            None => {
                let (driver, transcript) = self.read_final_triage_message_with_driver(&execution.id).await;
                match transcript.into_message() {
                    None => None,
                    Some(text) => {
                        let candidates = crate::driver_transcript::driver_or_default(driver.as_deref())
                            .structured_output_fallback(
                                crate::structured_output::StructuredOutputKind::ReviewResult,
                                &text,
                            );
                        let (result, err) = crate::pr_review::review_result_from_candidates(&candidates);
                        if let Some(ref e) = err {
                            tracing::warn!(
                                execution_id = %execution.id,
                                producing_task_id,
                                error = %e,
                                "pr_review finalize: transcript JSON block present but did not \
                                 validate as ReviewResult",
                            );
                            if parse_error.is_none() {
                                parse_error = err;
                            }
                        }
                        result
                    }
                }
            }
        };

        // Neither the artifact nor the transcript yielded a valid ReviewResult.
        // Do NOT silently advance the PR unreviewed (the old failure mode that
        // dropped every finding). Probe the still-live reviewer to (re-)write
        // its artifact and re-run the finalizer on the next Stop — bounded by
        // the shared auto-nudge breaker so a reviewer that never produces a
        // valid result cannot loop forever.
        if review_result.is_none() {
            match self.nudge_breaker.record(
                &execution.id,
                "pr_review:awaiting_result",
                self.max_unproductive_nudges,
                (self.now_fn)(),
            ) {
                NudgeDecision::TooSoon { since_last } => {
                    tracing::debug!(
                        execution_id = %execution.id,
                        producing_task_id,
                        since_last_ms = since_last.as_millis(),
                        "pr_review finalize: identical re-prompt suppressed (debounce) — waiting \
                         for the reviewer's next natural Stop before asking again",
                    );
                    return StopOutcome::ReviewPassAwaitingResult;
                }
                NudgeDecision::Proceed { count } => {
                    let is_remote = match self.work_db.execution_host_id(&execution.id) {
                        Ok(Some(host_id)) => host_id != "local",
                        Ok(None) => false,
                        Err(err) => {
                            tracing::warn!(execution_id = %execution.id, ?err, "pr_review finalize: could not resolve execution host for re-prompt; using local path");
                            false
                        }
                    };
                    let output_path = crate::structured_output::path_for(
                        &self.structured_output_dir,
                        &execution.id,
                        crate::structured_output::StructuredOutputKind::ReviewResult,
                    );
                    let output_destination = if is_remote {
                        "$BOSS_STRUCTURED_OUTPUT".to_owned()
                    } else {
                        output_path.display().to_string()
                    };
                    // Include the specific serde error in the probe when we have one so
                    // the reviewer can correct the exact malformation rather than blindly
                    // rewriting the entire JSON.
                    //
                    // Driver-agnostic on purpose: the probe must not name a driver-specific
                    // tool or assume that a previous file operation succeeded. Batch review
                    // members bypass this fallback entirely and fail explicitly when their
                    // proposal is absent.
                    let probe = if let Some(ref parse_err) = parse_error {
                        format!(
                            "Your review did not produce a valid ReviewResult. The JSON was \
                             present but failed to parse:\n\n  {parse_err}\n\n\
                             Correct the JSON so it matches the schema in your task prompt. \
                             Write it to this file:\n\n{}\n\n\
                             If your sandbox does not allow writing that path, instead end \
                             your reply with the corrected JSON in a fenced ```json block as \
                             the last content in the message. Do NOT change the PR.",
                            output_destination,
                        )
                    } else {
                        format!(
                            "Your review did not produce a valid ReviewResult. Write the \
                             ReviewResult JSON (matching the schema in your task prompt) to \
                             this file:\n\n{}\n\n\
                             If your sandbox does not allow writing that path, instead end \
                             your reply with the JSON in a fenced ```json block as the last \
                             content in the message. Do NOT change the PR.",
                            output_destination,
                        )
                    };
                    tracing::warn!(
                        execution_id = %execution.id,
                        producing_task_id,
                        nudge_count = count,
                        max = self.max_unproductive_nudges,
                        "pr_review finalize: no readable ReviewResult (artifact + transcript \
                         both empty/invalid); re-prompting reviewer to write the artifact",
                    );
                    self.probe_queuer.queue_probe(&execution.id, &probe);
                    return StopOutcome::ReviewPassAwaitingResult;
                }
                NudgeDecision::Trip { count } => {
                    tracing::error!(
                        execution_id = %execution.id,
                        producing_task_id,
                        nudge_count = count,
                        "pr_review finalize: reviewer failed to produce a valid ReviewResult \
                         after re-prompting; keeping the task in Doing and filing an attention",
                    );
                    self.file_review_result_giveup_attention(execution, count).await;
                    // Fall through with review_result = None. The completion
                    // write keeps the PendingReview hold so recovery can
                    // re-fire instead of presenting this as human-reviewable.
                }
            }
        }

        // We are going to finalise now (we have a result, or we gave up after
        // re-prompting). Reap the engine-owned artifact either way.
        crate::structured_output::clear_all(&self.structured_output_dir, &execution.id);

        // Extract head_sha before review_result is (potentially)
        // consumed by the revision path below. Used to update last_reviewed_sha.
        let head_sha_for_cycle: Option<String> = review_result
            .as_ref()
            .map(|r| r.head_sha.clone())
            .filter(|s| !s.is_empty());

        let original_revision_warranted = review_result
            .as_ref()
            .is_some_and(crate::pr_review::passes_severity_gate);

        // Tracked on the review-cycle root (chain root for a revision-
        // triggered pass, the task itself otherwise) so the counter
        // accumulates across the whole revision chain instead of resetting
        // to zero on every fresh revision task row — see
        // `WorkDb::review_cycle_root_id`. Depends only on task kind and
        // chain parentage, not on anything `record_worker_pr_completion`
        // writes, so it's safe to resolve ahead of that call.
        let cycle_root_id = self.work_db.review_cycle_root_id(producing_task_id);

        // incident-002 postmortem action item: rationale-independent
        // both-parents deletion tripwire. For a conflict-resolution review, diff the resolution
        // against BOTH merge parents; if it removed a surface a merged parent
        // added, halt auto-progression — the task is held in `blocked:
        // deletion_signoff` pending explicit operator sign-off instead of
        // advancing to human Review, regardless of the reviewer's verdict.
        let deletion_signoff = self
            .compute_merge_parent_deletion_signoff(producing_task_id, execution, head_sha_for_cycle.as_deref())
            .await;
        let completion_target = if !deletion_signoff.is_empty() {
            WorkerPrCompletionTarget::BlockedDeletionSignoff
        } else if review_result.is_some() {
            WorkerPrCompletionTarget::InReview
        } else {
            WorkerPrCompletionTarget::PendingReview
        };

        // Dedup at the revision-minting end too. If a prior COMPLETED
        // review pass already recorded this exact head sha as reviewed
        // (`last_reviewed_sha`), this pass is a redundant duplicate review of
        // unchanged code — e.g. two independent `pr_review` executions raced
        // past the enqueue-side guard (`WorkDb::create_pr_review_execution_dedup`)
        // before it existed, or a stale duplicate execution survived from
        // before this fix landed. Minting a second findings revision from it
        // would re-litigate content the first pass's revision already covers.
        // Read the state BEFORE `increment_task_review_cycle` overwrites it
        // with this pass's own (matching) sha — the "before_commit_sha ==
        // head_sha" signature pattern used elsewhere for re-fire guards
        // (see ci_watch's rebounce idempotency key). Read as late as possible
        // (right before that write, after the `.await` above) to keep the
        // window between this read and `increment_task_review_cycle`'s write
        // as narrow as possible — this is exactly the guard meant to catch
        // two `pr_review` executions racing past the enqueue-side dedup.
        let duplicate_head_review = match self.work_db.get_task_review_cycle_state(&cycle_root_id) {
            Ok((_, prior_sha)) => {
                head_sha_for_cycle.as_deref().is_some_and(|sha| !sha.is_empty())
                    && prior_sha.as_deref() == head_sha_for_cycle.as_deref()
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    producing_task_id,
                    cycle_root_id,
                    ?err,
                    "pr_review finalize: could not read prior review_cycle state; \
                     assuming not a duplicate head review",
                );
                false
            }
        };
        let revision_warranted = original_revision_warranted && !duplicate_head_review;

        // Durable per-pass review verdict. Without it,
        // `work_executions.status = 'completed'` is written identically for a
        // clean review and for one whose findings were computed and then
        // discarded, so stored state cannot distinguish them. `None` result
        // ⇒ the only way this point is reached with no `ReviewResult` is the
        // give-up path above (`NudgeDecision::Trip`); every other None-result
        // path already returned early. Findings computed then dropped by the
        // duplicate-head guard are recorded as such, not silently folded
        // into `completed_clean`.
        let findings_count = review_result.as_ref().map_or(0, |r| r.findings.len() as i64);
        let gate_outcome = if review_result.is_none() {
            crate::work::REVIEW_GATE_OUTCOME_GAVE_UP
        } else if duplicate_head_review && original_revision_warranted {
            crate::work::REVIEW_GATE_OUTCOME_DROPPED_DUPLICATE_HEAD
        } else if revision_warranted {
            crate::work::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS
        } else {
            crate::work::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN
        };
        // `revision_warranted` on the verdict is the gate's own (pre-dedup)
        // answer — see `ReviewVerdictInput::revision_warranted` — so it
        // stays `true` for a dropped-duplicate pass even though no revision
        // exists; `gate_outcome` is what tells that story.
        let review_verdict = crate::work::ReviewVerdictInput {
            head_sha: head_sha_for_cycle.clone(),
            findings_count,
            revision_warranted: original_revision_warranted,
            gate_outcome,
        };

        // Atomically: advance the producing task from active → in_review (or
        // hold it in blocked:deletion_signoff when the tripwire fired) +
        // complete the reviewer execution + clear its cube columns + record
        // the review verdict above. Same path for both revision and
        // no-revision cases.
        // Marked before the terminalizing write — see `super::teardown`.
        //
        // `pr_head_after` is intentionally `None` for reviewer teardowns:
        // the column's forensic purpose is severity of mid-turn reaps on
        // *producing* executions that lose work. Reviewers do not hold that
        // contribution surface; a post-teardown head on the review execution
        // would not answer the same query and would require a second gh call
        // on every review finalize.
        let teardown = self.begin_teardown(&execution.id);
        let completion = match self.work_db.record_worker_pr_completion(
            &execution.id,
            &pr_url,
            None,
            None,
            completion_target,
            Some(review_verdict),
        ) {
            Ok(Some(completion)) => completion,
            Ok(None) => return StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(
                    execution_id = %execution.id,
                    producing_task_id,
                    ?err,
                    "pr_review finalize: DB write failed",
                );
                return StopOutcome::DbError;
            }
        };

        // A fresh pass just completed for this work item (clean, with
        // findings, or gave-up) — any earlier `pr_review_died_without_findings`
        // attention (filed when a PREVIOUS review died before finishing) no
        // longer describes the current state, so resolve it. Best-effort:
        // this pass has already been durably recorded above regardless of
        // whether this cleanup succeeds.
        if let Err(err) = self.work_db.resolve_external_tracker_attention(
            producing_task_id,
            crate::pr_review_recovery::PR_REVIEW_DIED_ATTENTION_KIND,
        ) {
            tracing::warn!(
                execution_id = %execution.id,
                producing_task_id,
                ?err,
                "pr_review finalize: failed to resolve stale pr_review_died_without_findings attention",
            );
        }

        // Increment the review cycle counter and record last_reviewed_sha.
        // This happens regardless of whether a revision was warranted — the
        // cycle ticks on every completed reviewer pass. A failure here is
        // non-fatal (the task is already in in_review).
        if duplicate_head_review {
            tracing::warn!(
                execution_id = %execution.id,
                producing_task_id,
                cycle_root_id,
                head_sha = ?head_sha_for_cycle,
                trigger,
                "pr_review finalize: this pass reviewed a head sha a prior completed pass \
                 already recorded as reviewed; skipping review_cycle increment and findings \
                 revision to avoid minting a duplicate (duplicate-review guard)",
            );
        } else if let Err(err) = self
            .work_db
            .increment_task_review_cycle(&cycle_root_id, head_sha_for_cycle.as_deref())
        {
            tracing::warn!(
                execution_id = %execution.id,
                producing_task_id,
                cycle_root_id,
                ?err,
                "pr_review finalize: failed to increment review_cycle; \
                 cycle-bound enforcement may be off by one",
            );
        }

        // 2026-07-01 revision-review experiment: log the trigger kind and
        // wall-clock duration of this pass so the engine surfaces can count
        // cycles and time spent by trigger without a schema change.
        let duration_secs = execution
            .started_at
            .as_deref()
            .or(Some(execution.created_at.as_str()))
            .and_then(elapsed_secs_since);
        tracing::info!(
            execution_id = %execution.id,
            producing_task_id,
            trigger,
            duration_secs,
            "pr_review pass duration",
        );

        self.finish_worker_teardown(
            &execution.id,
            completion.released_lease_id.as_deref(),
            workspace_path.as_deref().map(std::path::Path::new),
            "pr_review",
            teardown,
        )
        .await;

        let product_id = completion.work_item.product_id().to_string();
        let work_item_id = work_item_id(&completion.work_item);

        // incident-002 postmortem gate: the deletion tripwire fired, so the task is now
        // held in `blocked: deletion_signoff`. File the operator sign-off
        // surface enumerating the removed merged-parent surfaces and stop — no
        // revision is created (deletion of merged code is an operator decision,
        // not an auto-remediation the pipeline should quietly attempt).
        if !deletion_signoff.is_empty() {
            let _ = self.work_db.create_attention_item(CreateAttentionItemInput {
                work_item_id: Some(work_item_id.clone()),
                kind: crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND.to_owned(),
                title: crate::merge_parent_deletion::SIGNOFF_ATTENTION_TITLE.to_owned(),
                body_markdown: crate::merge_parent_deletion::render_signoff_attention_body(&deletion_signoff, &pr_url),
                execution_id: None,
                status: None,
                resolved_at: None,
            });
            tracing::warn!(
                execution_id = %execution.id,
                producing_task_id,
                pr_url = %pr_url,
                removed = deletion_signoff.len(),
                trigger,
                "pr_review finalize: merge-parent deletion tripwire fired; task held \
                 in blocked:deletion_signoff pending operator sign-off",
            );
            self.publisher
                .publish(&execution.id, &work_item_id, "completed", "pr_review_deletion_signoff")
                .await;
            self.publisher
                .publish_work_item_changed(&product_id, &work_item_id, "pr_review_deletion_signoff")
                .await;
            return StopOutcome::ReviewPassCompleted { pr_url };
        }

        // If the severity gate passed, create a revision on the
        // producing task with the rendered findings as revision instructions.
        // The revision is dispatched on the general worker pool (autostart = true,
        // the default). Nothing is posted to GitHub — feedback stays inside Boss.
        if revision_warranted {
            // `review_result` is Some when `revision_warranted` is true.
            let result = review_result.expect("revision_warranted implies Some(ReviewResult)");
            // The origin is the chain root — the task itself for a first-pass
            // review, or the root of the revision chain for a re-review (same
            // row `cycle_root_id` above already resolved) — so the title and
            // description always point at the work item whose PR is actually
            // under review, not at an intermediate revision row.
            let origin_task_short_id = match self.work_db.get_work_item(&cycle_root_id) {
                Ok(WorkItem::Task(ref t)) | Ok(WorkItem::Chore(ref t)) => t.short_id,
                _ => None,
            };
            let origin = crate::pr_review::ReviewOrigin {
                task_short_id: origin_task_short_id,
                pr_number: pr_number_from_url(&pr_url).map(|n| n as i64),
            };
            let instructions = crate::pr_review::render_revision_instructions(&result, origin);
            let title = crate::pr_review::render_revision_title(origin, result.findings.len());
            let created_via = format!("{CREATED_VIA_PR_REVIEW_PREFIX}{}", execution.id);

            match self.work_db.create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(producing_task_id.clone())
                    .description(instructions)
                    .name(title)
                    .created_via(created_via)
                    // The reviewer already diagnosed each finding and
                    // enumerated the fix; the revision's job is to apply
                    // them, not to investigate. Pin `standard` rather than
                    // inheriting the chain root's mode.
                    .reasoning(ReasoningMode::Standard)
                    .build(),
                self.pr_state_checker.as_ref(),
            ) {
                Ok(revision) => {
                    if let Err(err) = self
                        .work_db
                        .set_review_verdict_revision_task_id(&execution.id, &revision.id)
                    {
                        tracing::warn!(
                            execution_id = %execution.id,
                            producing_task_id,
                            revision_task_id = %revision.id,
                            ?err,
                            "pr_review finalize: failed to record revision_task_id on the review verdict",
                        );
                    }
                    tracing::info!(
                        execution_id = %execution.id,
                        producing_task_id,
                        revision_task_id = %revision.id,
                        pr_url = %pr_url,
                        findings = result.findings.len(),
                        trigger,
                        "pr_review pass finalised; revision created for qualifying findings",
                    );
                    self.publisher
                        .publish(
                            &execution.id,
                            &work_item_id,
                            "completed",
                            "pr_review_pass_revision_created",
                        )
                        .await;
                    self.publisher
                        .publish_work_item_changed(&product_id, &work_item_id, "pr_review_pass_revision_created")
                        .await;
                    return StopOutcome::ReviewPassRevisionCreated {
                        pr_url,
                        revision_task_id: revision.id,
                    };
                }
                Err(err) => {
                    // Revision creation failed (parent no longer revisable — PR
                    // merged or closed between review and now). The producing task
                    // is already in in_review; fall through to ReviewPassCompleted.
                    // The findings this pass computed are about to be discarded —
                    // amend the verdict this transaction already wrote so that
                    // destruction leaves a trace instead of a silent
                    // `completed_with_findings` row with no revision to show for it.
                    if let Err(mark_err) = self.work_db.mark_review_verdict_revision_creation_failed(&execution.id) {
                        tracing::warn!(
                            execution_id = %execution.id,
                            producing_task_id,
                            ?mark_err,
                            "pr_review finalize: failed to record revision_creation_failed on the review verdict",
                        );
                    }
                    tracing::warn!(
                        execution_id = %execution.id,
                        producing_task_id,
                        ?err,
                        "pr_review finalize: create_revision failed (parent likely no longer \
                         revisable); advancing to in_review without revision",
                    );
                }
            }
        }

        self.publisher
            .publish(&execution.id, &work_item_id, "completed", "pr_review_pass_completed")
            .await;
        self.publisher
            .publish_work_item_changed(&product_id, &work_item_id, "pr_review_pass_completed")
            .await;

        tracing::info!(
            execution_id = %execution.id,
            producing_task_id,
            pr_url = %pr_url,
            trigger,
            "pr_review pass finalised; producing task advanced to in_review",
        );
        StopOutcome::ReviewPassCompleted { pr_url }
    }

    /// Finalise a review-batch leaf without consulting its artifact or
    /// transcript. A submitted report was already schema-validated and linked
    /// to the member by the synchronous proposal applier; no submission is an
    /// explicit member failure the batch recovery flow can retry by role.
    async fn finalize_review_batch_member(
        &self,
        execution: &crate::work::WorkExecution,
        member: crate::work::ReviewBatchMember,
    ) -> StopOutcome {
        let batch = match self.work_db.review_batch(&member.batch_id) {
            Ok(Some(batch)) => batch,
            Ok(None) => {
                tracing::error!(
                    execution_id = %execution.id,
                    batch_id = %member.batch_id,
                    "pr_review batch member has no persisted batch",
                );
                return StopOutcome::DbError;
            }
            Err(err) => {
                tracing::error!(
                    execution_id = %execution.id,
                    batch_id = %member.batch_id,
                    ?err,
                    "pr_review finalize: could not load batch",
                );
                return StopOutcome::DbError;
            }
        };

        let result_summary = match member.status {
            boss_protocol::ReviewBatchMemberStatus::Reported => "review report submitted",
            boss_protocol::ReviewBatchMemberStatus::Pending | boss_protocol::ReviewBatchMemberStatus::Running => {
                match self.work_db.fail_review_batch_member_for_execution(&execution.id) {
                    Ok(true) => {
                        tracing::warn!(
                            execution_id = %execution.id,
                            member_id = %member.id,
                            batch_id = %member.batch_id,
                            "reviewer stopped without submitting review-report proposal; member marked failed",
                        );
                        "review report proposal missing; member failed"
                    }
                    Ok(false) => "review batch member was already terminal",
                    Err(err) => {
                        tracing::error!(
                            execution_id = %execution.id,
                            member_id = %member.id,
                            ?err,
                            "pr_review finalize: could not mark missing proposal as member failure",
                        );
                        return StopOutcome::DbError;
                    }
                }
            }
            boss_protocol::ReviewBatchMemberStatus::Failed => "review batch member already failed",
        };

        // A batch member never reads the ReviewResult artifact — delivery is
        // exclusively via the `boss propose review-report` proposal above —
        // so it never reaches `collect_remote_structured_output`'s call
        // sites, and for a remote reviewer that means its
        // `~/.boss-remote/runs/<id>.structured-output-path` descriptor and
        // `$TMPDIR` artifact are never reaped: reaping is otherwise a side
        // effect of a successful pull. Trigger that same pull-and-reap here,
        // purely for its reap side effect (the pulled copy is immediately
        // discarded by `clear_all` below); best-effort, since this member is
        // already finalized via the proposal channel regardless of outcome.
        match self
            .collect_remote_structured_output(execution, crate::structured_output::StructuredOutputKind::ReviewResult)
            .await
        {
            RemoteCollectionResult::Failed { host_id, reason } => {
                tracing::warn!(
                    execution_id = %execution.id,
                    host_id = %host_id,
                    error = %reason,
                    "pr_review batch member: remote structured-output reap failed; \
                     descriptor/artifact may be left behind on the host",
                );
            }
            RemoteCollectionResult::NotRemote
            | RemoteCollectionResult::Collected
            | RemoteCollectionResult::NotAvailable => {}
        }

        // The report body is only a shell-safe input file for `boss propose`.
        // Once the proposal is accepted (or the member is failed), retaining a
        // local copy must not create a second, artifact-based completion path.
        crate::structured_output::clear_all(&self.structured_output_dir, &execution.id);

        let lease_id = execution.cube_lease_id.clone();
        let workspace_path = execution.workspace_path.clone();
        let teardown = self.begin_teardown(&execution.id);
        match self
            .work_db
            .complete_pane_parked_execution(&execution.id, "completed", Some(result_summary))
        {
            Ok(Some(_)) => {}
            Ok(None) => return StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(
                    execution_id = %execution.id,
                    ?err,
                    "pr_review finalize: could not complete batch member execution",
                );
                return StopOutcome::DbError;
            }
        }
        self.finish_worker_teardown(
            &execution.id,
            lease_id.as_deref(),
            workspace_path.as_deref().map(std::path::Path::new),
            "pr_review_batch_member",
            teardown,
        )
        .await;
        self.publisher
            .publish(
                &execution.id,
                &execution.work_item_id,
                "completed",
                "pr_review_batch_member",
            )
            .await;
        StopOutcome::ReviewPassCompleted { pr_url: batch.pr_url }
    }

    /// incident-002 postmortem: compute the rationale-independent both-parents deletion
    /// tripwire for a conflict-resolution review.
    ///
    /// Returns rendered description lines for each merged-parent surface the
    /// resolution removed. Empty when the reviewed PR is not a conflict
    /// resolution, the resolution has no recorded parents / did not succeed, the
    /// repo slug is unresolvable, or the resolution preserved every
    /// merged-parent surface. Fail-open on any GitHub error (see
    /// [`crate::merge_parent_deletion::compute_merged_parent_deletions`]).
    pub(super) async fn compute_merge_parent_deletion_signoff(
        &self,
        producing_task_id: &str,
        execution: &crate::work::WorkExecution,
        reviewed_head: Option<&str>,
    ) -> Vec<String> {
        // The `conflict_resolutions` row is keyed on the review-cycle root (the
        // original in-review task), not the revision that pushed the fix.
        let root = self.work_db.review_cycle_root_id(producing_task_id);
        let cr = match self.work_db.latest_conflict_resolution_for_work_item(&root) {
            Ok(Some(cr)) => cr,
            Ok(None) => return Vec::new(),
            Err(err) => {
                tracing::warn!(
                    producing_task_id,
                    root,
                    ?err,
                    "pr_review finalize: conflict_resolution lookup failed; \
                     skipping merge-parent deletion tripwire",
                );
                return Vec::new();
            }
        };
        // Only gate a resolution whose worker actually pushed a fix. `pending`
        // has not pushed; `failed`/`abandoned` bailed without a resolution.
        // (`running`/`succeeded` bracket the push — the poller marks
        // `succeeded` only on a later retirement sweep, which can race this
        // review, so we accept `running` too.)
        if !matches!(cr.status.as_str(), "running" | "succeeded") {
            return Vec::new();
        }
        // The resolved head is the head the reviewer just reviewed; fall back to
        // the recorded `head_sha_after` (set at retirement). The other two
        // parents come from the attempt ledger.
        let head_after = reviewed_head.filter(|s| !s.is_empty()).or(cr.head_sha_after.as_deref());
        let (Some(head_before), Some(base_sha), Some(head_after)) = (
            cr.head_sha_before.as_deref(),
            cr.base_sha_at_trigger.as_deref(),
            head_after,
        ) else {
            return Vec::new();
        };
        let repo_slug = match parse_repo_slug(&execution.repo_remote_url) {
            Ok(slug) => slug,
            Err(_) => return Vec::new(),
        };
        crate::merge_parent_deletion::compute_merged_parent_deletions(&repo_slug, head_before, base_sha, head_after)
            .await
    }

    /// Read the final assistant text of `execution_id`'s transcript, if any.
    /// Returns `None` when no transcript is recorded/readable or it contains
    /// no assistant turn — the caller treats that as "no decision".
    /// Read a finished triage execution's final assistant message from its
    /// transcript, returning a [`TriageTranscript`] that distinguishes the
    /// failure-to-read cases (no path / unreadable / no assistant prose) from a
    /// successful read. The caller folds these states into the run-history
    /// `detail` so a `failed_will_retry` triage row is diagnosable instead of
    /// collapsing to a bare "no decision marker".
    ///
    /// Every entry is normalized through the run's own driver before parsing
    /// (see [`crate::driver_transcript`]) — this reader is what the
    /// Stop-boundary marker scans (`[blocked]`, `[effort-escalation]`,
    /// `[deferred-scope]`, `NO_CHANGES_NEEDED`), the triage-decision fallback
    /// and the PR-URL prose fallback all read through, so parsing the file as
    /// if every agent wrote Claude's dialect made all of them silently
    /// Claude-only. For Claude the normalization is the identity.
    ///
    /// Retries the read with a short bounded backoff (see
    /// [`TRIAGE_TRANSCRIPT_READ_ATTEMPTS`]) when the transcript parses but
    /// yields no assistant text. This closes a Stop-boundary flush race: the
    /// Stop hook can fire — and trigger this finaliser — within milliseconds
    /// of the worker's final assistant-text line being written, before the
    /// transcript writer has flushed that line (and the `stop_hook_summary`
    /// / `turn_duration` lines after it) to disk. A single synchronous read
    /// in that window sees a transcript that ends exactly at the turn before
    /// the marker and permanently mis-finalises a correct `skip`/`task`
    /// decision as `failed_will_retry` (field incident: transcript readback
    /// found 12 events — precisely the pre-final-message count — while the
    /// durable file on disk had 15, the 13th being the missing assistant
    /// text). Re-reading the same durable path a few times catches the write
    /// once it lands instead of racing it once.
    pub(super) async fn read_final_triage_message(&self, execution_id: &str) -> TriageTranscript {
        self.read_final_triage_message_with_driver(execution_id).await.1
    }

    /// [`Self::read_final_triage_message`], additionally handing back the
    /// driver it resolved while reading. Callers that need both the message
    /// text and the driver (the `pr_review` and PR-URL-prose fallbacks, which
    /// both feed the text into that same driver's `structured_output_fallback`)
    /// should call this instead of `read_final_triage_message` followed by a
    /// second, separate [`crate::driver_transcript::driver_for_execution`] —
    /// the driver lookup is a DB round trip, and this function already pays
    /// it once.
    pub(super) async fn read_final_triage_message_with_driver(
        &self,
        execution_id: &str,
    ) -> (Option<std::sync::Arc<dyn crate::driver::AgentDriver>>, TriageTranscript) {
        let driver = crate::driver_transcript::driver_for_execution(&self.work_db, execution_id);
        let transcript = self
            .read_final_triage_message_inner(execution_id, driver.as_deref())
            .await;
        (driver, transcript)
    }

    async fn read_final_triage_message_inner(
        &self,
        execution_id: &str,
        driver: Option<&dyn crate::driver::AgentDriver>,
    ) -> TriageTranscript {
        let path = match self.work_db.transcript_path_for_execution(execution_id) {
            Ok(Some(path)) => path,
            Ok(None) => {
                tracing::warn!(
                    execution_id,
                    "triage finalisation: no transcript path recorded; treating as no decision",
                );
                return TriageTranscript::NoPath;
            }
            Err(err) => {
                tracing::warn!(execution_id, ?err, "triage finalisation: transcript lookup failed",);
                return TriageTranscript::Unreadable;
            }
        };

        // `driver` is resolved once by the caller (see
        // `read_final_triage_message_with_driver`), outside the retry loop:
        // the run's driver cannot change between attempts, and the lookup is
        // a DB round trip.
        let mut last_event_count = 0usize;
        let mut last_content_len = 0usize;
        for attempt in 1..=TRIAGE_TRANSCRIPT_READ_ATTEMPTS {
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(content) => content,
                Err(err) => {
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "triage finalisation: failed to read transcript file",
                    );
                    return TriageTranscript::Unreadable;
                }
            };
            let events = crate::driver_transcript::parse_transcript_with_driver(driver, &content);
            // Collect ALL assistant text turns, not just the last one.
            //
            // The triage agent emits its decision marker in the turn AFTER the
            // `boss task create` Bash call.  The Stop hook can fire before that
            // post-tool turn is fully flushed to disk, so `iter().rev().find_map`
            // (which returned only the last AssistantText) would land on the
            // pre-tool analysis message — which has no marker — and record
            // `failed_will_retry` even though the task was successfully created.
            //
            // Joining all turns mirrors `attentions_detector::extract_assistant_text`
            // and ensures the marker is found regardless of which turn contains it.
            // The "exactly one marker" contract still holds: `parse_triage_decision`
            // enforces it across the combined text.
            let all_text: Vec<String> = events
                .iter()
                .filter_map(|e| match &e.kind {
                    crate::transcript_markdown::TranscriptEventKind::AssistantText(t) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            if !all_text.is_empty() {
                if attempt > 1 {
                    tracing::info!(
                        execution_id,
                        attempt,
                        "triage finalisation: assistant text appeared on retry (Stop-boundary flush race recovered)",
                    );
                }
                tracing::debug!(
                    execution_id,
                    transcript_bytes = content.len(),
                    event_count = events.len(),
                    assistant_turns = all_text.len(),
                    "triage finalisation: read all assistant turns for marker scan",
                );
                return TriageTranscript::FinalMessage(all_text.join("\n"));
            }
            last_event_count = events.len();
            last_content_len = content.len();
            if attempt < TRIAGE_TRANSCRIPT_READ_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_millis(
                    TRIAGE_TRANSCRIPT_READ_RETRY_BASE_MS * u64::from(attempt),
                ))
                .await;
            }
        }
        // `driver` names which normalizer ran: a non-empty transcript that
        // still yields no assistant turn is the shape of a dialect the parse
        // path does not understand, and that is indistinguishable at every
        // downstream marker scan from "the worker said nothing". Log which
        // driver wrote it so the ambiguity is attributable rather than silent.
        tracing::warn!(
            execution_id,
            driver = driver.map(|driver| driver.descriptor().name).unwrap_or("(unresolved)"),
            transcript_bytes = last_content_len,
            event_count = last_event_count,
            attempts = TRIAGE_TRANSCRIPT_READ_ATTEMPTS,
            "triage finalisation: transcript had no assistant text event after flush-race retries",
        );
        TriageTranscript::NoAssistantText {
            event_count: last_event_count,
        }
    }

    /// Evaluate whether `producing`'s push is a conflict-resolution or
    /// CI-fix result that changed nothing but the base it sits on — a
    /// *pure* rebase, with no authored content and no manually-altered
    /// conflict hunk. Returns [`PureRebaseGateOutcome::skip_reason`] as
    /// `Some("pure_rebase")` when so; `None` when `producing` isn't a
    /// conflict/CI-fix resolution, there's nothing on record to compare
    /// against, or any GitHub call fails (fail open: an unproven predicate
    /// must never suppress a real review). `post_head` is populated
    /// whenever this gate got as far as fetching the PR's current head
    /// OID, so [`Self::check_noop_skip`] can reuse it instead of issuing a
    /// second, identical `fetch_pr_head_oid` call.
    ///
    /// Deliberately independent of `review_cycle` / `last_reviewed_sha`
    /// (unlike [`Self::check_noop_skip`]) so it also covers a
    /// conflict/CI-fix push that lands before the PR's very first review —
    /// `check_noop_skip`'s rule 1 always treats that as "never skip", which
    /// is right for genuinely new content but wrong for a rebase that
    /// contributes none.
    ///
    /// # The predicate
    ///
    /// Let `pre_head` be the PR head immediately before this resolution
    /// began, read off the `conflict_resolutions` / `ci_remediations`
    /// attempt row whose `revision_task_id` is `producing.work_item_id` —
    /// i.e. the specific attempt that spawned this revision, not merely
    /// the freshest row on record for the review-cycle root (a fresher
    /// attempt or a non-pushing `retrigger` row can otherwise win and
    /// supply the wrong `pre_head`). Let `post_head` be the PR's current
    /// head, and `base_ref` the PR's target-branch ref name: for a
    /// conflict resolution this is `cr.base_branch`, already stamped on
    /// the attempt row; `ci_remediations` rows never stamp a base branch
    /// (a CI failure isn't necessarily caused by a base move), so it's
    /// fetched live via [`BranchVerifier::fetch_pr_base_ref`].
    ///
    /// The push is a pure rebase iff the file-level diff of
    /// `base_ref...pre_head` (what the PR contributes before this
    /// resolution, per GitHub's three-dot `compare` semantics — i.e.
    /// against `merge_base(base_ref, pre_head)`) is byte-identical to the
    /// file-level diff of `base_ref...post_head` (what it contributes
    /// now). Comparing each head against the SAME live base ref — rather
    /// than a single fixed pre-rebase commit — is what makes the two
    /// sides comparable: a three-dot compare against a fixed point
    /// silently pulls in every commit the base moved through between
    /// `pre_head` and `post_head`, which would make the two diffs differ
    /// on *any* base movement, not just on hand-authored content. Any real
    /// difference at all — new lines, a hand-resolved conflict hunk, a
    /// dropped file — still fails the predicate and a full review runs; a
    /// resolution that mostly rebases but also hand-edits a few lines to
    /// make it compile is NOT purely a rebase.
    ///
    /// On success, appends a `[pr-review-skip]` audit line to the work
    /// item's description (mirrors the `[deferred-scope]` /
    /// `[engine-reconcile]` convention) so an operator asking "why did
    /// this PR never get an AI review pass" has a durable answer on the
    /// item itself, not just a log line.
    pub(super) async fn check_pure_rebase_skip(
        &self,
        pr_url: &str,
        producing: &crate::work::WorkExecution,
        cycle_root_id: &str,
    ) -> PureRebaseGateOutcome {
        let none = PureRebaseGateOutcome::no_skip();
        let task = match self.work_db.get_work_item(&producing.work_item_id) {
            Ok(WorkItem::Task(t) | WorkItem::Chore(t)) => t,
            Ok(_) => return none,
            Err(err) => {
                tracing::warn!(
                    work_item_id = %producing.work_item_id,
                    ?err,
                    "pure-rebase skip: work item lookup failed; proceeding with review",
                );
                return none;
            }
        };
        let created_via = task.created_via.clone();
        // `conflict_resolutions` / `ci_remediations` rows are keyed on the
        // review-cycle root (the original in-review task), not the
        // revision task that actually pushed the fix — mirrors
        // `compute_merge_parent_deletion_signoff`. Within that root's rows,
        // select the one whose `revision_task_id` is THIS revision — the
        // freshest row overall can belong to a different (possibly later,
        // possibly non-pushing) attempt.
        let root = cycle_root_id;
        let (pre_head, base_ref) = if created_via.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX) {
            let cr = match self.work_db.list_conflict_resolutions(None, &[], Some(root), None) {
                Ok(rows) => rows
                    .into_iter()
                    .find(|r| r.revision_task_id.as_deref() == Some(producing.work_item_id.as_str())),
                Err(err) => {
                    tracing::warn!(work_item_id = %root, ?err, "pure-rebase skip: conflict_resolution lookup failed; proceeding with review");
                    return none;
                }
            };
            let Some(cr) = cr else { return none };
            match cr.head_sha_before.filter(|s| !s.is_empty()) {
                Some(pre_head) => (pre_head, Some(cr.base_branch)),
                None => return none,
            }
        } else if created_via.starts_with(CREATED_VIA_CI_FIX_PREFIX) {
            let attempt = match self.work_db.list_ci_remediations(None, &[], Some(root), None) {
                Ok(rows) => rows
                    .into_iter()
                    .find(|r| r.revision_task_id.as_deref() == Some(producing.work_item_id.as_str())),
                Err(err) => {
                    tracing::warn!(work_item_id = %root, ?err, "pure-rebase skip: ci_remediation lookup failed; proceeding with review");
                    return none;
                }
            };
            let Some(attempt) = attempt else { return none };
            match Some(crate::work::merge_queue_rebounce_pr_head(&attempt.head_sha_at_trigger).to_owned())
                .filter(|s| !s.is_empty())
            {
                // `ci_remediations` never stamps a base branch — resolved
                // live below via `fetch_pr_base_ref`.
                Some(pre_head) => (pre_head, None),
                None => return none,
            }
        } else {
            // Not a conflict-resolution / CI-fix push — the predicate
            // doesn't apply; fall through to the general no-op gate.
            return none;
        };

        let repo_slug = match parse_repo_slug(&producing.repo_remote_url) {
            Ok(slug) => slug,
            Err(err) => {
                tracing::warn!(
                    repo_remote_url = %producing.repo_remote_url,
                    ?err,
                    "pure-rebase skip: cannot parse repo slug; proceeding with review",
                );
                return none;
            }
        };
        let Some(pr_number) = pr_number_from_url(pr_url) else {
            tracing::warn!(
                pr_url,
                "pure-rebase skip: cannot parse PR number; proceeding with review"
            );
            return none;
        };

        let post_head = match self.branch_verifier.fetch_pr_head_oid(&repo_slug, pr_number).await {
            Ok(sha) => sha,
            Err(err) => {
                tracing::warn!(
                    pr_url,
                    ?err,
                    "pure-rebase skip: cannot fetch PR head OID; proceeding with review"
                );
                return none;
            }
        };
        if post_head == pre_head {
            // Nothing pushed this round — `check_noop_skip`'s
            // `sha_unchanged` rule covers this once it also has a
            // `last_reviewed_sha` to compare against.
            return PureRebaseGateOutcome::no_skip_with_head(post_head);
        }

        let base_ref = match base_ref {
            Some(base) => base,
            None => match self.branch_verifier.fetch_pr_base_ref(&repo_slug, pr_number).await {
                Ok(base) => base,
                Err(err) => {
                    tracing::warn!(
                        pr_url,
                        ?err,
                        "pure-rebase skip: cannot fetch PR base ref; proceeding with review",
                    );
                    return PureRebaseGateOutcome::no_skip_with_head(post_head);
                }
            },
        };

        let diff_before = match self
            .branch_verifier
            .fetch_diff_signature(&repo_slug, &base_ref, &pre_head)
            .await
        {
            Ok(sig) => sig,
            Err(err) => {
                tracing::warn!(
                    pr_url,
                    base_ref,
                    pre_head,
                    ?err,
                    "pure-rebase skip: cannot fetch pre-resolution diff signature; proceeding with review",
                );
                return PureRebaseGateOutcome::no_skip_with_head(post_head);
            }
        };
        let diff_after = match self
            .branch_verifier
            .fetch_diff_signature(&repo_slug, &base_ref, &post_head)
            .await
        {
            Ok(sig) => sig,
            Err(err) => {
                tracing::warn!(
                    pr_url,
                    base_ref,
                    post_head,
                    ?err,
                    "pure-rebase skip: cannot fetch post-resolution diff signature; proceeding with review",
                );
                return PureRebaseGateOutcome::no_skip_with_head(post_head);
            }
        };

        if diff_before != diff_after {
            return PureRebaseGateOutcome::no_skip_with_head(post_head);
        }

        // Recorded on `root` (the parent chore / chain root), not
        // `producing.work_item_id` (the revision task) — the root is the
        // PR-owning card an operator actually looks at, and the same item
        // `review_cycle` / `last_reviewed_sha` are tracked on.
        self.record_pure_rebase_skip(root, &created_via, &pre_head, &post_head);
        PureRebaseGateOutcome {
            skip_reason: Some("pure_rebase"),
            post_head: Some(post_head),
        }
    }

    /// Best-effort `[pr-review-skip]` audit line — see
    /// [`Self::check_pure_rebase_skip`]. A failure here never blocks the
    /// skip decision itself; it is logged and swallowed, mirroring
    /// `append_reconcile_audit_best_effort`.
    fn record_pure_rebase_skip(&self, work_item_id: &str, created_via: &str, pre_head: &str, post_head: &str) {
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let line = format!(
            "\n[pr-review-skip] epoch {now}: reason=pure_rebase created_via={created_via} \
             pre_head={pre_head} post_head={post_head} — automated review skipped: the resolution's \
             diff against its pre-resolution base is byte-identical before and after, i.e. nothing \
             changed but the base.",
        );
        if let Err(err) = crate::reconcile_audit::append_description_line(&self.work_db, work_item_id, &line) {
            tracing::warn!(
                work_item_id,
                ?err,
                "pure-rebase skip: audit-line append failed (non-fatal)",
            );
        }
    }

    /// Evaluate the no-op / trivial-diff skip gate for the automated reviewer.
    ///
    /// Returns `Some(reason)` when the reviewer pass should be skipped,
    /// or `None` when a full review is warranted.
    ///
    /// Rules, in order:
    /// 1. If `review_cycle == 0` or `last_reviewed_sha` is `None` → first
    ///    review → never skip (design: "first review of a PR is never skipped
    ///    by the trivial rule").
    /// 2. If the current PR head OID equals `last_reviewed_sha` → skip
    ///    (`"sha_unchanged"`): the worker pushed the exact same commit.
    /// 3. If the effective diff between `last_reviewed_sha` and the current
    ///    head is 0 changed lines → skip (`"empty_diff"`): pure rebase with
    ///    no file-content changes.
    /// 4. If `min_review_changed_lines > 0` and the diff is below that
    ///    threshold → skip (`"trivial_diff"`): cosmetically small push.
    ///
    /// API errors during steps 2–4 are logged and treated as "don't skip"
    /// so the reviewer still runs on uncertainty.
    ///
    /// `head_oid_hint` lets a caller that already fetched the PR's current
    /// head OID this Stop cycle (e.g. [`Self::check_pure_rebase_skip`],
    /// whose `PureRebaseGateOutcome::post_head` carries it) pass it
    /// straight in, avoiding a second identical `fetch_pr_head_oid` round
    /// trip for the same PR. `None` when no gate ran ahead of this one.
    pub(super) async fn check_noop_skip(
        &self,
        pr_url: &str,
        producing: &crate::work::WorkExecution,
        review_cycle: i64,
        last_reviewed_sha: Option<&str>,
        head_oid_hint: Option<String>,
    ) -> Option<&'static str> {
        let Some(last_sha) = last_reviewed_sha else {
            return None; // first review
        };
        if review_cycle == 0 {
            return None; // first review (belt-and-suspenders; last_sha is None when cycle=0)
        }

        // Parse repo slug and PR number for GitHub API calls.
        let repo_slug = match parse_repo_slug(&producing.repo_remote_url) {
            Ok(slug) => slug,
            Err(err) => {
                tracing::warn!(
                    repo_remote_url = %producing.repo_remote_url,
                    ?err,
                    "pr_review noop gate: cannot parse repo slug; proceeding with review",
                );
                return None;
            }
        };
        let Some(pr_number) = pr_number_from_url(pr_url) else {
            tracing::warn!(
                pr_url,
                "pr_review noop gate: cannot parse PR number; proceeding with review",
            );
            return None;
        };

        // Fetch current PR head SHA, unless the pure-rebase gate already
        // fetched it this cycle.
        let current_head = match head_oid_hint {
            Some(sha) => sha,
            None => match self.branch_verifier.fetch_pr_head_oid(&repo_slug, pr_number).await {
                Ok(sha) => sha,
                Err(err) => {
                    tracing::warn!(
                        pr_url,
                        ?err,
                        "pr_review noop gate: cannot fetch PR head OID; proceeding with review",
                    );
                    return None;
                }
            },
        };

        // Rule 2: exact SHA match — nothing changed since last review.
        if current_head == last_sha {
            return Some("sha_unchanged");
        }

        // Rules 3 & 4: compare effective diff between last-reviewed head and
        // current head. Fail open on API errors.
        let diff_lines = match self
            .branch_verifier
            .fetch_diff_line_count(&repo_slug, last_sha, &current_head)
            .await
        {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(
                    pr_url,
                    last_reviewed_sha = last_sha,
                    current_head = %current_head,
                    ?err,
                    "pr_review noop gate: cannot fetch diff line count; proceeding with review",
                );
                return None;
            }
        };

        if diff_lines == 0 {
            return Some("empty_diff");
        }

        if self.min_review_changed_lines > 0 && diff_lines < self.min_review_changed_lines {
            return Some("trivial_diff");
        }

        None
    }
}

/// Outcome of [`WorkerCompletionHandler::collect_remote_structured_output`],
/// carrying the host id alongside the adapter's
/// [`crate::host_adapter::CollectOutcome`] so a terminal failure can name
/// which host it came from.
pub(super) enum RemoteCollectionResult {
    /// Not a remote execution, no host to collect from — the ordinary
    /// local-artifact / transcript read path applies unchanged.
    NotRemote,
    Collected,
    /// Not available yet, or a transient condition (DB error, missing
    /// provider, transport failure) prevented the attempt — not proof the
    /// artifact is bad. Callers must fall through to transcript/nudge
    /// recovery, not terminalize.
    NotAvailable,
    /// Positive proof the artifact on `host_id` cannot be trusted.
    Failed {
        host_id: String,
        reason: String,
    },
}

impl WorkerCompletionHandler {
    /// Collect a remote worker's structured-output artifact, fully fail-open
    /// except for [`RemoteCollectionResult::Failed`]: only that variant is
    /// positive proof (an invalid descriptor, or a copy that ran and failed)
    /// that the result cannot be trusted. Every other obstacle — the host
    /// lookup failing, the host row vanishing, the adapter provider being
    /// unavailable, a transport error resolving the adapter — is a
    /// transient/engine-side condition, not evidence about the artifact
    /// itself, so it degrades to `NotAvailable` and lets the caller fall
    /// back to transcript/nudge recovery instead of terminalizing a
    /// completed review over (for example) a momentary ControlMaster hiccup.
    pub(super) async fn collect_remote_structured_output(
        &self,
        execution: &crate::work::WorkExecution,
        kind: crate::structured_output::StructuredOutputKind,
    ) -> RemoteCollectionResult {
        let host_id = match self.work_db.execution_host_id(&execution.id) {
            Ok(Some(host_id)) if host_id != "local" => host_id,
            Ok(_) => return RemoteCollectionResult::NotRemote,
            Err(err) => {
                tracing::warn!(execution_id = %execution.id, ?err, "structured-output collection: host lookup failed; falling back to normal completion");
                return RemoteCollectionResult::NotAvailable;
            }
        };
        let host = match self.work_db.get_host(&host_id) {
            Ok(Some(host)) => host,
            Ok(None) => {
                tracing::warn!(execution_id = %execution.id, host_id, "structured-output collection: remote host is no longer registered; falling back to normal completion");
                return RemoteCollectionResult::NotAvailable;
            }
            Err(err) => {
                tracing::warn!(execution_id = %execution.id, host_id, ?err, "structured-output collection: host lookup failed; falling back to normal completion");
                return RemoteCollectionResult::NotAvailable;
            }
        };
        let provider = match self
            .host_adapter_provider
            .read()
            .expect("host adapter provider lock poisoned")
            .clone()
        {
            Some(provider) => provider,
            None => {
                tracing::warn!(execution_id = %execution.id, host_id, "structured-output collection: remote host adapter provider is unavailable; falling back to normal completion");
                return RemoteCollectionResult::NotAvailable;
            }
        };
        let adapter = match provider.adapter_for(&host).await {
            Ok(adapter) => adapter,
            Err(err) => {
                tracing::warn!(execution_id = %execution.id, host_id, ?err, "structured-output collection: could not resolve host adapter; falling back to normal completion");
                return RemoteCollectionResult::NotAvailable;
            }
        };
        let destination = crate::structured_output::path_for(&self.structured_output_dir, &execution.id, kind);
        match adapter
            .collect_structured_output(&execution.id, kind, &destination)
            .await
        {
            Ok(crate::host_adapter::CollectOutcome::Collected) => RemoteCollectionResult::Collected,
            Ok(crate::host_adapter::CollectOutcome::NotAvailable) => RemoteCollectionResult::NotAvailable,
            Ok(crate::host_adapter::CollectOutcome::Failed(reason)) => {
                RemoteCollectionResult::Failed { host_id, reason }
            }
            Err(err) => {
                tracing::warn!(execution_id = %execution.id, host_id, ?err, "structured-output collection: adapter call failed; falling back to normal completion");
                RemoteCollectionResult::NotAvailable
            }
        }
    }
}
