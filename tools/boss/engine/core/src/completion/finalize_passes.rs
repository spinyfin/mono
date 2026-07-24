//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

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
    /// The worker was told to end its final message with exactly one of
    /// `automation: task <id>` or `automation: skip — <reason>`. Steps:
    /// 1. read the final assistant message and parse the decision;
    /// 2. for a `task` marker, verify the id resolves to a task carrying this
    ///    automation's provenance — so a misbehaving agent can't pass off an
    ///    unrelated task as its own output;
    /// 3. record the terminal outcome (`produced_task` / `skipped`, or keep
    ///    `failed_will_retry` for a missing / ambiguous / unverifiable marker);
    /// 4. finalise the execution (`completed`) and release pane + workspace.
    pub(super) async fn finalize_automation_triage(&self, execution: &crate::work::WorkExecution) -> StopOutcome {
        let automation_id = execution.work_item_id.clone();
        let transcript = self.read_final_triage_message(&execution.id).await;
        let decision = match &transcript {
            TriageTranscript::FinalMessage(text) => parse_triage_decision(text),
            // No path / unreadable / no assistant prose all mean we have no
            // message to scan for a marker — treat as NoDecision, but the
            // specific transcript state is folded into the detail below so the
            // run history distinguishes "ran but emitted no marker" from
            // "produced no transcript at all".
            TriageTranscript::NoPath | TriageTranscript::Unreadable | TriageTranscript::NoAssistantText { .. } => {
                TriageDecision::NoDecision
            }
        };

        let (outcome, produced_task_id, detail): (&str, Option<String>, Option<String>) = match &decision {
            TriageDecision::ProducedTask(marker_id) => {
                match self.work_db.get_work_item_resolving_short_id(marker_id) {
                    Ok(Some(WorkItem::Task(t))) | Ok(Some(WorkItem::Chore(t)))
                        if t.source_automation_id.as_deref() == Some(automation_id.as_str()) =>
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
                // Recovery path: if the worker ran `boss task create --automation`
                // but forgot to emit the decision marker (or emitted it in a turn
                // the Stop hook raced past), the open-task record is the ground
                // truth. Treat the most recently created open task as the
                // produced outcome rather than recording `failed_will_retry`.
                //
                // Without this, every retry creates another task until the
                // open-task cap fills, then loops as `failed_will_retry` forever
                // — the exact "Fix compilation warnings: at limit 3/3" wedge in
                // the field evidence.
                match self.work_db.find_most_recent_open_task_for_automation(&automation_id) {
                    Ok(Some(task)) => {
                        tracing::warn!(
                            execution_id = %execution.id,
                            automation_id = %automation_id,
                            recovered_task_id = %task.id,
                            base_detail,
                            "triage run ended without a valid decision marker but \
                             found an open task produced by this automation; \
                             recording as produced_task (marker-recovery path \
                             prevents duplicate tasks on retry)",
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
                            tracing::warn!(
                                execution_id = %execution.id,
                                automation_id = %automation_id,
                                base_detail,
                                "triage run created no task and emitted no skip marker, but its \
                                 final message plainly concluded there is nothing to do; recording \
                                 as skipped (skip marker-recovery) instead of failed_will_retry",
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
                        None => (AUTOMATION_OUTCOME_FAILED_WILL_RETRY, None, Some(base_detail)),
                    },
                    Err(err) => {
                        tracing::warn!(
                            execution_id = %execution.id,
                            automation_id = %automation_id,
                            ?err,
                            "triage recovery: DB query for open tasks failed; \
                             recording as failed_will_retry",
                        );
                        (AUTOMATION_OUTCOME_FAILED_WILL_RETRY, None, Some(base_detail))
                    }
                }
            }
        };

        match self.work_db.finalize_automation_triage_run(
            &execution.id,
            outcome,
            produced_task_id.as_deref(),
            detail.as_deref(),
        ) {
            Ok(true) => {}
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
        if let Some(lease_id) = lease_id.as_deref()
            && let Err(err) = self.cube_client.release_workspace(lease_id).await
        {
            tracing::error!(
                execution_id = %execution.id,
                lease_id,
                ?err,
                "triage finalisation: cube workspace release failed",
            );
        }
        self.pane_releaser.release_pane(&execution.id).await;
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
                if let Err(err) = self
                    .work_db
                    .recover_unanswered_comment(&comment_id, Some(&run.id), "no_reply_posted")
                {
                    tracing::warn!(
                        execution_id = %execution.id,
                        run_id = %run.id,
                        ?err,
                        "answer-agent finalizer: failed to recover the comment from its unanswered run",
                    );
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
        if let Some(lease_id) = lease_id.as_deref()
            && let Err(err) = self.cube_client.release_workspace(lease_id).await
        {
            tracing::error!(
                execution_id = %execution.id,
                lease_id,
                ?err,
                "answer-agent finalisation: cube workspace release failed",
            );
        }
        self.pane_releaser.release_pane(&execution.id).await;
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
        // via `ReviewResult::from_json`. TRANSITIONAL FALLBACK: scrape the
        // transcript's final message (fenced / bare JSON) — this covers remote
        // workers, whose artifact is written on the remote host and not
        // readable here, and any local artifact-write failure. The legacy
        // scraper (`extract_review_result` + the balanced-brace hack) is kept
        // only as this fallback and can be deleted once the artifact path is
        // proven in production.
        //
        // `last_parse_error` captures the serde error from the last failed
        // parse attempt across both channels so it can be included verbatim
        // in the reviewer re-prompt, giving the reviewer the specific field +
        // type message rather than a generic "write valid JSON" instruction.
        let mut last_parse_error: Option<String> = None;

        let from_artifact = match crate::structured_output::read(&self.structured_output_dir, &execution.id) {
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
                         validate as ReviewResult; trying the transcript fallback",
                    );
                    last_parse_error = Some(err_str);
                    None
                }
            },
        };
        let review_result = match from_artifact {
            Some(result) => Some(result),
            None => match self.read_final_triage_message(&execution.id).await.into_message() {
                None => None,
                Some(text) => {
                    let (result, err) = crate::pr_review::extract_review_result_verbose(&text);
                    if let Some(ref e) = err {
                        tracing::warn!(
                            execution_id = %execution.id,
                            producing_task_id,
                            error = %e,
                            "pr_review finalize: transcript JSON block present but did not \
                             validate as ReviewResult",
                        );
                        if last_parse_error.is_none() {
                            last_parse_error = err;
                        }
                    }
                    result
                }
            },
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
                    let output_path = crate::structured_output::path_in(&self.structured_output_dir, &execution.id);
                    // Include the specific serde error in the probe when we have one so
                    // the reviewer can correct the exact malformation rather than blindly
                    // rewriting the entire JSON.
                    let probe = if let Some(ref parse_err) = last_parse_error {
                        format!(
                            "Your review did not produce a valid ReviewResult. The JSON was \
                             present but failed to parse:\n\n  {parse_err}\n\n\
                             Correct the JSON so it matches the schema in your task prompt, \
                             write it to this file with the Write tool, then stop — do NOT \
                             change the PR:\n\n{}",
                            output_path.display(),
                        )
                    } else {
                        format!(
                            "Your review did not produce a valid ReviewResult. Write the \
                             ReviewResult JSON (matching the schema in your task prompt) to \
                             this file with the Write tool, then stop — do NOT change the PR:\n\n{}",
                            output_path.display(),
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
                         after re-prompting; advancing to in_review WITHOUT a revision and \
                         filing an attention",
                    );
                    self.file_review_result_giveup_attention(execution, count).await;
                    // Fall through with review_result = None → advance to
                    // in_review unimpeded (no revision).
                }
            }
        }

        // We are going to finalise now (we have a result, or we gave up after
        // re-prompting). Reap the engine-owned artifact either way.
        crate::structured_output::clear(&self.structured_output_dir, &execution.id);

        // Extract head_sha before review_result is (potentially)
        // consumed by the revision path below. Used to update last_reviewed_sha.
        let head_sha_for_cycle: Option<String> = review_result
            .as_ref()
            .map(|r| r.head_sha.clone())
            .filter(|s| !s.is_empty());

        let revision_warranted = review_result
            .as_ref()
            .is_some_and(crate::pr_review::passes_severity_gate);

        // incident-002 postmortem action item: rationale-independent
        // both-parents deletion tripwire. For a conflict-resolution review, diff the resolution
        // against BOTH merge parents; if it removed a surface a merged parent
        // added, halt auto-progression — the task is held in `blocked:
        // deletion_signoff` pending explicit operator sign-off instead of
        // advancing to human Review, regardless of the reviewer's verdict.
        let deletion_signoff = self
            .compute_merge_parent_deletion_signoff(producing_task_id, execution, head_sha_for_cycle.as_deref())
            .await;
        let completion_target = if deletion_signoff.is_empty() {
            WorkerPrCompletionTarget::InReview
        } else {
            WorkerPrCompletionTarget::BlockedDeletionSignoff
        };

        // Atomically: advance the producing task from active → in_review (or
        // hold it in blocked:deletion_signoff when the tripwire fired) +
        // complete the reviewer execution + clear its cube columns. Same path
        // for both revision and no-revision cases.
        let completion = match self
            .work_db
            .record_worker_pr_completion(&execution.id, &pr_url, None, completion_target)
        {
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

        // Increment the review cycle counter and record
        // last_reviewed_sha. This happens regardless of whether a revision
        // was warranted — the cycle ticks on every completed reviewer pass.
        // A failure here is non-fatal (the task is already in in_review).
        //
        // Tracked on the review-cycle root (chain root for a revision-
        // triggered pass, the task itself otherwise) so the counter
        // accumulates across the whole revision chain instead of resetting
        // to zero on every fresh revision task row — see
        // `WorkDb::review_cycle_root_id`.
        let cycle_root_id = self.work_db.review_cycle_root_id(producing_task_id);

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
        // (see ci_watch's rebounce idempotency key).
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
        let revision_warranted = revision_warranted && !duplicate_head_review;

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

        if let Some(lease_id) = completion.released_lease_id.as_deref()
            && let Err(err) = self.cube_client.release_workspace(lease_id).await
        {
            tracing::error!(
                execution_id = %execution.id,
                lease_id,
                ?err,
                "pr_review finalize: cube workspace release failed",
            );
        }
        self.pane_releaser.release_pane(&execution.id).await;

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
            let instructions = crate::pr_review::render_revision_instructions(&result);
            let created_via = format!("{CREATED_VIA_PR_REVIEW_PREFIX}{}", execution.id);

            match self.work_db.create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(producing_task_id.clone())
                    .description(instructions)
                    .created_via(created_via)
                    .build(),
                self.pr_state_checker.as_ref(),
            ) {
                Ok(revision) => {
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
            let events = crate::transcript_markdown::parse_transcript(&content);
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
        tracing::warn!(
            execution_id,
            transcript_bytes = last_content_len,
            event_count = last_event_count,
            attempts = TRIAGE_TRANSCRIPT_READ_ATTEMPTS,
            "triage finalisation: transcript had no assistant text event after flush-race retries",
        );
        TriageTranscript::NoAssistantText {
            event_count: last_event_count,
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
    pub(super) async fn check_noop_skip(
        &self,
        pr_url: &str,
        producing: &crate::work::WorkExecution,
        review_cycle: i64,
        last_reviewed_sha: Option<&str>,
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

        // Fetch current PR head SHA.
        let current_head = match self.branch_verifier.fetch_pr_head_oid(&repo_slug, pr_number).await {
            Ok(sha) => sha,
            Err(err) => {
                tracing::warn!(
                    pr_url,
                    ?err,
                    "pr_review noop gate: cannot fetch PR head OID; proceeding with review",
                );
                return None;
            }
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
