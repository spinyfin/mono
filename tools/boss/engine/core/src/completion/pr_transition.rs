//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

#[derive(serde::Deserialize)]
struct BatchReviewPrView {
    #[serde(rename = "baseRefOid")]
    base_sha: String,
    #[serde(rename = "headRefOid")]
    target_sha: String,
    #[serde(default)]
    additions: Option<i64>,
    #[serde(default)]
    deletions: Option<i64>,
}

/// Fetch and freeze the target metadata before handing it to the atomic DB
/// creation path. Callers can retain the legacy path if this cannot establish
/// an immutable target SHA.
async fn enqueue_review_batch(
    work_db: &crate::work::WorkDb,
    producing: &crate::work::WorkExecution,
    pr_url: &str,
) -> anyhow::Result<crate::work::ReviewBatchDispatch> {
    let root =
        boss_github::pr_files::fetch_pr_view_json(pr_url, "baseRefOid,headRefOid,files,additions,deletions").await?;
    let view: BatchReviewPrView = serde_json::from_value(root.clone())?;
    if view.base_sha.is_empty() || view.target_sha.is_empty() {
        anyhow::bail!("GitHub PR metadata omitted immutable base or head SHA");
    }
    let pr_number = boss_github::pr_url::pr_number_from_url(pr_url)
        .ok_or_else(|| anyhow::anyhow!("could not parse pull request number from {pr_url:?}"))?
        .try_into()
        .map_err(|_| anyhow::anyhow!("pull request number does not fit the review-batch schema"))?;
    let classification = crate::pr_review::classify_pr_review_metadata(&crate::pr_review::PrReviewMetadata {
        additions: view.additions,
        changed_files: Some(boss_github::pr_files::parse_changed_file_paths(&root)),
        deletions: view.deletions,
    });
    let input = crate::work::ReviewBatchCreateInput::builder()
        .cycle_root_id(work_db.review_cycle_root_id(&producing.work_item_id))
        .base_sha(view.base_sha)
        .classification(classification)
        .phase(boss_protocol::ReviewBatchPhase::PreMerge)
        .pr_number(pr_number)
        .pr_url(pr_url)
        .target_sha(view.target_sha)
        .build();
    work_db.create_pre_merge_review_batch(input, &producing.repo_remote_url)
}

impl WorkerCompletionHandler {
    /// Stop hook path, `"pr_recheck"` for the merge-poller's
    /// fallback sweep — so operators can see which path closed a
    /// given chore.
    pub(super) async fn finalize_pr_transition(
        &self,
        execution_id: &str,
        pr_url: String,
        target: WorkerPrCompletionTarget,
        source: &'static str,
    ) -> StopOutcome {
        // Read once and reuse below for the reviewer-enqueue check, so both
        // spots agree on the same execution snapshot instead of racing two
        // independent reads. Captured before `record_worker_pr_completion`
        // below nulls `workspace_path` in the same transaction that
        // terminalizes the execution — this IS the path that terminalizes a
        // parked-live (`waiting_human` / `running`) execution on PR success,
        // so it owns driver teardown; nothing downstream of this call reaps
        // it. A DB error here is distinct from a legitimately-absent path —
        // teardown still runs per this function's contract, but the failure
        // must be loud rather than silently collapsed into `None`.
        let execution_result = self.work_db.get_execution(execution_id);
        let workspace_path = match &execution_result {
            Ok(execution) => execution.workspace_path.clone(),
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "pr completion: could not read workspace_path for driver teardown; \
                     tearing down with None",
                );
                None
            }
        };

        let merged = matches!(target, WorkerPrCompletionTarget::Done);

        // incident-004 AI-3: a revision may terminalize to PendingReview /
        // InReview only with an explicit associated change (head SHA moved
        // attributably, or observed PR metadata mutation) or an explicit
        // nothing-to-do outcome. Silence after a mid-turn reap — no push,
        // no metadata marker, no NO_CHANGES_NEEDED — must not read as
        // review-ready. Couples the existing `sha_unchanged` observation
        // (reviewer noop skip) to status success; runs before mid-turn
        // reap accounting so a refused finalize never looks like a reap.
        if !merged
            && matches!(target, WorkerPrCompletionTarget::InReview)
            && let Ok(producing) = execution_result.as_ref()
            && producing.kind == ExecutionKind::RevisionImplementation
        {
            match self
                .evaluate_revision_review_contribution(execution_id, producing, &pr_url, source)
                .await
            {
                RevisionReviewGate::Allow(reason) => {
                    if reason == RevisionContributionReason::Indeterminate {
                        tracing::warn!(
                            execution_id,
                            pr_url = %pr_url,
                            source,
                            contribution = reason.as_str(),
                            "revision contribution gate: allowing InReview terminalization with \
                             NO usable SHA-delta / metadata / no-op evidence — contribution was \
                             never established (missing baseline or head fetch failed)",
                        );
                    } else {
                        tracing::info!(
                            execution_id,
                            pr_url = %pr_url,
                            source,
                            contribution = reason.as_str(),
                            "revision contribution gate: allowing InReview terminalization",
                        );
                    }
                }
                RevisionReviewGate::Refuse { reason } => {
                    tracing::warn!(
                        execution_id,
                        pr_url = %pr_url,
                        source,
                        refuse_reason = reason,
                        "revision contribution gate: refusing InReview / PendingReview \
                         terminalization — no head movement, no metadata-fix confirmation, and \
                         no explicit NO_CHANGES_NEEDED (couples sha_unchanged to status success; \
                         incident-004 AI-3)",
                    );
                    return StopOutcome::AwaitingInput;
                }
            }
        }

        // Every PR-terminalization source funnels through here, so this is
        // the one place that can prevent a recheck or Stop race from reaping
        // a worker that is still mid-turn. Read BEFORE
        // `begin_teardown`/`finish_worker_teardown` can release the pane and
        // clear the live-state slot out from under this check.
        if let Ok(execution) = execution_result.as_ref()
            && self.observed_mid_turn(execution_id)
        {
            if self.should_defer_mid_turn_pr_completion(execution_id, source) {
                tracing::info!(
                    execution_id,
                    source,
                    kind = %execution.kind,
                    "pr completion: worker is still mid-turn; deferring terminalization to its activity boundary",
                );
                return StopOutcome::AwaitingInput;
            }
            self.record_mid_turn_reap(execution, source).await;
        }

        // Forensic teardown head — captured before any durable reviewer
        // enqueue or terminalization write. Fail-open: this snapshot is not
        // consulted by the mid-turn guard, so a transient `gh` failure or an
        // unparseable URL must not block PR completion engine-wide.
        let pr_head_after = match execution_result.as_ref() {
            Ok(execution) => self.fetch_pr_head_after(execution, &pr_url).await,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    source,
                    ?err,
                    "pr completion: cannot capture pr_head_after without an execution record; \
                     continuing terminalization with None",
                );
                None
            }
        };

        // For reviewer-triggering executions with a fresh
        // (non-merged) PR, try to enqueue an independent reviewer pass
        // instead of immediately advancing to human Review.
        // This also checks the cycle bound first — if review_cycle has
        // already reached max_review_cycles, skip the reviewer and proceed
        // to InReview with a sticky attention item for the human.
        // If the pr_review execution cannot be created (DB error), fall back
        // to the normal InReview path so the task is never left stuck.
        let enqueued_reviewer = if !merged && matches!(target, WorkerPrCompletionTarget::InReview) {
            match execution_result.as_ref() {
                Ok(producing)
                    if should_enqueue_reviewer_for_primary(&producing.kind)
                        || (producing.kind == ExecutionKind::RevisionImplementation
                            && self.enable_revision_triggered_reviews) =>
                {
                    // 2026-07-01 revision-review experiment: distinguish the
                    // trigger for logging/observability (kill-switch gates
                    // only the revision arm above; a primary push is always
                    // reviewed).
                    let trigger = if producing.kind == ExecutionKind::RevisionImplementation {
                        "revision_push"
                    } else {
                        "primary_push"
                    };
                    // Read cycle state once — used by both
                    // the no-op gate and the cycle-bound check below.
                    // Tracked on the review-cycle root (chain root for a
                    // revision, the task itself otherwise) so cycle-bound and
                    // no-op-skip state accumulates across the whole revision
                    // chain instead of resetting on every fresh revision task
                    // row — see `WorkDb::review_cycle_root_id`.
                    let max_cycles = self.max_review_cycles;
                    let cycle_root_id = self.work_db.review_cycle_root_id(&producing.work_item_id);
                    let (review_cycle, last_reviewed_sha) =
                        match self.work_db.get_task_review_cycle_state(&cycle_root_id) {
                            Ok(state) => state,
                            Err(err) => {
                                // Fail open: treat as cycle=0, no prior SHA so both
                                // gates pass through (don't skip on uncertainty).
                                tracing::warn!(
                                    execution_id,
                                    work_item_id = %producing.work_item_id,
                                    cycle_root_id,
                                    ?err,
                                    "could not read review_cycle; assuming bound not reached",
                                );
                                (0i64, None)
                            }
                        };

                    // No-op / trivial-diff skip gate. Runs before
                    // the cycle-bound check so a pure rebase doesn't consume a
                    // cycle slot or surface an attention item.
                    //
                    // `check_pure_rebase_skip` runs first: it is keyed off the
                    // conflict/CI-fix attempt row rather than
                    // `review_cycle`/`last_reviewed_sha`, so it also catches a
                    // pure rebase that lands before the PR's very first review
                    // — a case `check_noop_skip`'s rule 1 always treats as
                    // "never skip" (right for genuinely new content, wrong for
                    // a rebase that contributes none). It is a no-op for any
                    // producer that isn't a conflict-resolution / CI-fix push.
                    let pure_rebase_gate = self.check_pure_rebase_skip(&pr_url, producing, &cycle_root_id).await;
                    let noop_skip_reason = match pure_rebase_gate.skip_reason {
                        Some(reason) => Some(reason),
                        None => {
                            self.check_noop_skip(
                                &pr_url,
                                producing,
                                review_cycle,
                                last_reviewed_sha.as_deref(),
                                pure_rebase_gate.post_head,
                            )
                            .await
                        }
                    };

                    if let Some(skip_reason) = noop_skip_reason {
                        tracing::info!(
                            execution_id,
                            work_item_id = %producing.work_item_id,
                            skip_reason,
                            trigger,
                            "pr_review noop skip: advancing to in_review without reviewer pass",
                        );
                        false
                    } else {
                        // Cycle bound check.
                        let cycle_bound_reached = (review_cycle as usize) >= max_cycles;

                        if cycle_bound_reached {
                            tracing::info!(
                                execution_id,
                                work_item_id = %producing.work_item_id,
                                max_review_cycles = max_cycles,
                                trigger,
                                "pr_review cycle bound reached; skipping reviewer \
                                 and advancing to in_review",
                            );
                            // Surface a sticky attention item so the human can see
                            // the cycle limit was hit when they open the PR card.
                            let _ = self.work_db.create_attention_item(CreateAttentionItemInput {
                                work_item_id: Some(producing.work_item_id.clone()),
                                kind: "pr_review_cycle_bound".to_owned(),
                                title: format!("Automated reviewer: cycle limit ({max_cycles}) reached"),
                                body_markdown: format!(
                                    "The automated reviewer completed {max_cycles} \
                                     cycle(s) on this PR without resolving all findings. \
                                     The PR has been advanced to human Review.\n\n\
                                     See the most recent revision task for the outstanding \
                                     findings from the last automated review cycle."
                                ),
                                execution_id: None,
                                status: None,
                                resolved_at: None,
                            });
                            false
                        } else {
                            // Dedup-and-insert atomically, closing the race where
                            // two independent completion triggers (the Stop-hook path
                            // and the merge-poller's `pr_recheck` sweep) each observe
                            // the producing execution as not-yet-terminal around the
                            // same moment and would otherwise both enqueue a `pr_review`
                            // execution for the same unchanged head sha.
                            if self.feature_flags.is_enabled("review_batch_fanout") {
                                match enqueue_review_batch(&self.work_db, producing, &pr_url).await {
                                    Ok(crate::work::ReviewBatchDispatch::Created { batch, executions }) => {
                                        tracing::info!(
                                            execution_id,
                                            batch_id = %batch.id,
                                            leaf_executions = executions.len(),
                                            pr_url = %pr_url,
                                            producing_kind = %producing.kind,
                                            trigger,
                                            "review batch leaf executions enqueued; holding producing task for reviewer pass",
                                        );
                                        self.publisher.kick_scheduler();
                                        true
                                    }
                                    Ok(crate::work::ReviewBatchDispatch::ExistingBatch { batch, executions }) => {
                                        tracing::info!(
                                            execution_id,
                                            batch_id = %batch.id,
                                            leaf_executions = executions.len(),
                                            pr_url = %pr_url,
                                            "review batch already enqueued for immutable target",
                                        );
                                        true
                                    }
                                    Ok(crate::work::ReviewBatchDispatch::LegacyExecution(review_exec)) => {
                                        tracing::info!(
                                            execution_id,
                                            review_execution_id = %review_exec.id,
                                            pr_url = %pr_url,
                                            "legacy reviewer already owns this target; preserving mode separation",
                                        );
                                        true
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            execution_id,
                                            ?error,
                                            "failed to create immutable review batch; using legacy reviewer path",
                                        );
                                        match self.work_db.create_pr_review_execution_dedup(
                                            &producing.work_item_id,
                                            &producing.repo_remote_url,
                                        ) {
                                            Ok((review_exec, _)) => {
                                                self.publisher.kick_scheduler();
                                                tracing::info!(
                                                    execution_id,
                                                    review_execution_id = %review_exec.id,
                                                    "legacy reviewer enqueued after batch metadata failure",
                                                );
                                                true
                                            }
                                            Err(legacy_error) => {
                                                tracing::warn!(
                                                    execution_id,
                                                    ?legacy_error,
                                                    "failed to create legacy reviewer after batch metadata failure",
                                                );
                                                false
                                            }
                                        }
                                    }
                                }
                            } else {
                                match self.work_db.create_pr_review_execution_dedup(
                                    &producing.work_item_id,
                                    &producing.repo_remote_url,
                                ) {
                                    Ok((review_exec, true)) => {
                                        tracing::info!(
                                            execution_id,
                                            review_execution_id = %review_exec.id,
                                            pr_url = %pr_url,
                                            producing_kind = %producing.kind,
                                            trigger,
                                            "pr_review execution enqueued; \
                                             holding producing task for reviewer pass",
                                        );
                                        self.publisher.kick_scheduler();
                                        true
                                    }
                                    Ok((review_exec, false)) => {
                                        tracing::info!(
                                            execution_id,
                                            review_execution_id = %review_exec.id,
                                            pr_url = %pr_url,
                                            producing_kind = %producing.kind,
                                            trigger,
                                            "pr_review execution already enqueued/in-flight for this \
                                             item; reusing instead of dispatching a duplicate review",
                                        );
                                        true
                                    }
                                    Err(err) => {
                                        tracing::warn!(
                                            execution_id,
                                            ?err,
                                            "failed to create pr_review execution; \
                                     falling back to immediate in_review",
                                        );
                                        false
                                    }
                                }
                            }
                        }
                    } // closes the `} else {` for the noop skip gate
                }
                Ok(_) => false, // non-reviewer-triggering execution; advance to in_review as normal
                Err(err) => {
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "could not load execution for reviewer-enqueue check; \
                         falling back to immediate in_review",
                    );
                    false
                }
            }
        } else {
            false
        };

        let effective_target = if enqueued_reviewer {
            WorkerPrCompletionTarget::PendingReview
        } else {
            target
        };

        // Mark the teardown in flight BEFORE the write that terminalizes
        // this execution. From here until `finish_worker_teardown` drops
        // the guard, `terminal_work_sweep` can see that this pane already
        // has an owner and must not reclaim it — without the mark, the
        // window between "row is terminal" and "pane is gone" is exactly
        // that sweep's reap precondition, and on 2026-07-30 it reaped and
        // SIGKILLed live workers inside it. Dropped automatically on both
        // early returns below.
        let teardown = self.begin_teardown(execution_id);

        let record_started = std::time::Instant::now();
        let completion = match self.work_db.record_worker_pr_completion(
            execution_id,
            &pr_url,
            pr_head_after.as_deref(),
            None,
            effective_target,
            None,
        ) {
            Ok(Some(completion)) => completion,
            Ok(None) => return StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(execution_id, source, ?err, "pr completion: failed to record");
                return StopOutcome::DbError;
            }
        };
        tracing::info!(
            execution_id,
            source,
            target = ?effective_target,
            elapsed_ms = record_started.elapsed().as_millis(),
            "pr completion: execution terminalized; teardown in flight",
        );
        // Clear the staged URL now that the DB write succeeded.
        // Deliberately ordered after `record_worker_pr_completion` so a
        // failed DB write leaves the cache intact for the worker's next
        // Stop. For primary implementations the next merge-poller sweep
        // also retries from this URL; a revision's sweep defers instead,
        // while its worker is observed mid-turn within
        // `staged_pr_mid_turn_defer_secs` of `staged_at` — once that horizon
        // expires (or the worker is not observed live), it recovers from
        // the exact head stamped by the Stop that observed the push (see
        // `revision_stop_contributed_head` and `recheck_for_pr`'s exact-head
        // gate).
        self.staged_pr_urls.forget(execution_id);
        // The worker contributed a PR — reset any accumulated nudge
        // count so a later unrelated nudge cycle starts clean.
        self.nudge_breaker.forget(execution_id);
        self.build_wait_tracker.forget(execution_id);
        self.background_children_tracker.forget(execution_id);
        self.hold_registry.release(execution_id);
        // Stop → pr_transition termination path: the call above just moved
        // the execution to `completed` (see `record_worker_pr_completion`),
        // so this path owns the whole teardown — pane, driver state, cube
        // lease, in that order. See `super::teardown` for why the ordering
        // is what it is (in particular why driver teardown must not run
        // while the worker process may still be alive).
        self.finish_worker_teardown(
            execution_id,
            completion.released_lease_id.as_deref(),
            workspace_path.as_deref().map(std::path::Path::new),
            source,
            teardown,
        )
        .await;
        let product_id = completion.work_item.product_id().to_string();
        let work_item_id = work_item_id(&completion.work_item);
        let publish_reason = match (merged, source) {
            (true, "pr_recheck") => "worker_pr_merged_recheck",
            (false, "pr_recheck") => "worker_pr_completed_recheck",
            (true, _) => "worker_pr_merged",
            (false, _) => "worker_pr_completed",
        };
        self.publisher
            .publish(
                &completion.execution.id,
                &completion.execution.work_item_id,
                completion.execution.status.as_str(),
                publish_reason,
            )
            .await;
        self.publisher
            .publish_work_item_changed(&product_id, &work_item_id, publish_reason)
            .await;
        // Doc-link auto-population. Two independent pointers:
        //  - every leaf work item -> the TASK's own `doc_*` pointer
        //    (`design_detector::on_task_doc_pr_*`). Independent of kind.
        //  - design tasks WITH a project (and design_postmortem) ALSO
        //    populate the PROJECT's `design_doc_*` pointer
        //    (`design_detector::on_design_pr_*`). That is a separate
        //    concept; this change does not merge the two.
        // The routing decision is logged ABOVE the per-branch dispatch so a
        // skip/proceed is ALWAYS visible without entering a gated block.
        // Detector errors are logged inside the detector — they must not
        // surface here because they'd mask the successful PR transition.
        //
        // `design_postmortem` completions skip the generic best-effort
        // followups mechanism entirely (see the guard below) in favour of
        // the mandatory, stronger `postmortem_followups` path — computed
        // once here so both spots agree on the kind check.
        let is_design_postmortem = matches!(
            &completion.work_item,
            WorkItem::Task(t) | WorkItem::Chore(t) if t.kind == TaskKind::DesignPostmortem
        );
        if let WorkItem::Task(ref task) | WorkItem::Chore(ref task) = completion.work_item {
            let produces_project_design =
                matches!(task.kind, TaskKind::Design | TaskKind::DesignPostmortem) && task.project_id.is_some();
            let decision = if produces_project_design {
                "per-task-doc+project-design-doc"
            } else {
                "per-task-doc"
            };
            tracing::info!(
                execution_id,
                work_item_id = %task.id,
                kind = %task.kind,
                project_id = ?task.project_id,
                merged,
                decision,
                "doc-detection: routing PR completion"
            );

            if merged {
                // Worker merged directly during its session; the detector
                // fetches base_ref_name from the PR (unknown here).
                design_detector::on_task_doc_pr_merged(&self.work_db, &task.id, &task.product_id, &pr_url, None).await;
            } else {
                design_detector::on_task_doc_pr_detected(&self.work_db, &task.id, &task.product_id, &pr_url).await;
            }
            // The earlier `publish_work_item_changed` above ran BEFORE
            // this detector call, so the client's refetch it triggers
            // can race the doc-pointer write and land the doc link
            // absent — leaving it visible only when some later,
            // unrelated event happens to refetch the tree. Publish a
            // second invalidation now that the pointer write (or its
            // no-op) has actually completed, mirroring the manual
            // `boss task set-doc` path (`app/work_items.rs`, reason
            // `task_doc_pointer_set`).
            self.publisher
                .publish_work_item_changed(&task.product_id, &task.id, "task_doc_pointer_set")
                .await;
        }

        // Per-project design-doc pointer + design-doc questions pipeline:
        // `kind=design` tasks WITH a project, and `kind=design_postmortem`
        // tasks (always project-scoped) which re-sync the same pointer
        // after editing the project's existing design doc.
        if let WorkItem::Task(ref task) | WorkItem::Chore(ref task) = completion.work_item
            && matches!(task.kind, TaskKind::Design | TaskKind::DesignPostmortem)
            && let Some(ref project_id) = task.project_id
        {
            if merged {
                // Worker merged directly during its session; update
                // the branch to main (base_ref_name unknown here,
                // so the detector will fetch it from the PR).
                design_detector::on_design_pr_merged(
                    &self.work_db,
                    &task.id,
                    &task.product_id,
                    project_id,
                    &pr_url,
                    None,
                )
                .await;

                // Postmortem-surfaced uncompleted work must become real
                // task rows, not a mention in the doc — see
                // `postmortem_followups` for why this is a stronger,
                // mandatory-artifact path distinct from the generic
                // best-effort `FOLLOWUPS:` mechanism below (which is
                // skipped entirely for this kind, see that block).
                if task.kind == TaskKind::DesignPostmortem {
                    crate::postmortem_followups::reconcile_postmortem_followups(
                        &self.work_db,
                        &task.id,
                        &task.product_id,
                        project_id,
                        execution_id,
                        Some(&self.structured_output_dir),
                    )
                    .await;
                }
            } else {
                design_detector::on_design_pr_detected(&self.work_db, &task.id, &task.product_id, project_id, &pr_url)
                    .await;
            }
            // See the matching comment on the per-task doc branch above:
            // the earlier `publish_work_item_changed` predates this
            // detector's pointer write, so publish a follow-up
            // invalidation now that it has actually landed.
            self.publisher
                .publish_work_item_changed(&task.product_id, &task.id, "design_doc_pointer_set")
                .await;

            // Attentions creation pipeline (design: attentions.md).
            // A design worker may ship a sibling `<slug>.attentions.json`
            // question manifest; parse it off the PR branch and upsert
            // the question group. Idempotent across re-detections.
            let questions_result = attentions_detector::reconcile_design_doc_questions(
                &self.work_db,
                &task.id,
                project_id,
                &pr_url,
                merged,
            )
            .await;
            if let Some((ref group, ref created)) = questions_result {
                self.publish_attentions_created(group, created).await;
            } else if self.feature_flags.is_enabled("attentions_questions_backstop") {
                // Primary found no manifest; fall back to the extraction
                // backstop which reads the doc's "Risks / open questions"
                // section (flagged `confidence_source = extracted`).
                if let Some((group, created)) = attentions_detector::extract_doc_questions_backstop(
                    &self.work_db,
                    &task.id,
                    project_id,
                    &pr_url,
                    merged,
                )
                .await
                {
                    self.publish_attentions_created(&group, &created).await;
                }
            }
        }

        // Followups: any completing implementation worker may surface
        // out-of-scope follow-on work. PRIMARY (design implementation task
        // 10): a `followup_task` proposal — `boss propose followup-task`
        // already upserted the member into the `followup` attention group
        // synchronously at submission time
        // (`crate::work::proposal_apply::stage_followup_task_in_transaction`),
        // for each follow-up submitted that way. LEGACY (counted fallback for
        // whatever a proposal did not already cover, or unconditionally when
        // the seam is off): the engine-owned structured-output artifact (a
        // `FollowupEntry` JSON array), falling back to a `FOLLOWUPS:` block
        // scraped from the transcript tail, and finally the
        // `attentions_followups_backstop` LLM pass. A no-op (no artifact / no
        // transcript / no block) when absent; idempotent across re-runs via
        // the store's content dedup.
        //
        // Follow-ups are inherently multi-item — the prompt sanctions a
        // worker mixing channels within one run (e.g. `boss propose
        // followup-task` for item 1, then falling back to the artifact for
        // item 2 if the CLI call fails) — so unlike the worker-signal /
        // deferred-scope seams, this is NOT a single execution-scoped
        // skip-or-run gate: [`WorkDb::reconcile_attentions`]' member-level
        // `content_key` dedup (keyed on `proposed_name` for the `followup`
        // kind) already runs against every existing member of the group,
        // proposal-staged members included, so the legacy chain always runs
        // and lands only the entries a proposal did not already stage —
        // exactly the "filter to uncovered entries" behaviour, driven off
        // the same group the proposal path writes into rather than a
        // separate in-memory predicate.
        //
        // Skipped for `design_postmortem`: its worker prompt never asks for
        // a `FollowupEntry`-shaped artifact (only the stronger
        // `postmortem_followups` schema above), so this would only ever
        // find nothing (or, if reusing the same artifact path, fail to
        // parse it as `FollowupEntry` and log spurious noise).
        if !is_design_postmortem {
            let followup_proposals_first = self.feature_flags.is_enabled("worker_proposals")
                && self.feature_flags.is_enabled("followup_proposals_seam");
            let transcript_path = self.work_db.transcript_path_for_execution(execution_id).ok().flatten();
            let followups_result = attentions_detector::reconcile_task_followups(
                &self.work_db,
                &work_item_id,
                execution_id,
                Some(&self.structured_output_dir),
                transcript_path.as_deref(),
            )
            .await;
            if let Some((ref group, ref created)) = followups_result {
                if followup_proposals_first {
                    for _ in created {
                        self.record_followup_fallback_hit(&completion.execution, "reconcile_task_followups");
                    }
                }
                self.publish_attentions_created(group, created).await;
            } else if self.feature_flags.is_enabled("attentions_followups_backstop") {
                // Primary found no FOLLOWUPS: block; fall back to the supervisor
                // extraction backstop (flagged `confidence_source = extracted`).
                if let Some((group, created)) = attentions_detector::extract_followups_backstop(
                    &self.work_db,
                    &work_item_id,
                    execution_id,
                    transcript_path.as_deref(),
                )
                .await
                {
                    if followup_proposals_first {
                        for _ in &created {
                            self.record_followup_fallback_hit(&completion.execution, "attentions_followups_backstop");
                        }
                    }
                    self.publish_attentions_created(&group, &created).await;
                }
            }
        }
        // Reap every engine-owned structured-output artifact this execution
        // produced (followups, PR URL) regardless of outcome — they live in
        // the system temp dir, but delete eagerly rather than waiting on OS
        // reaping.
        crate::structured_output::clear_all(&self.structured_output_dir, execution_id);

        if merged {
            tracing::info!(
                execution_id,
                work_item_id = %work_item_id,
                pr_url = %pr_url,
                source,
                "pr completion: PR already merged; moved work item to done"
            );
            StopOutcome::PrMerged { pr_url }
        } else if enqueued_reviewer {
            tracing::info!(
                execution_id,
                work_item_id = %work_item_id,
                pr_url = %pr_url,
                source,
                "pr completion: PR detected; reviewer enqueued — \
                 producing task held in active pending review pass",
            );
            StopOutcome::ReviewerEnqueued { pr_url }
        } else {
            tracing::info!(
                execution_id,
                work_item_id = %work_item_id,
                pr_url = %pr_url,
                source,
                "pr completion: PR detected; moved work item to in_review"
            );
            // Pre-fetch CI status so the Review card has a real icon from
            // the first frame. The fetch is fire-and-forget: if it fails or
            // the probe is slow the UI falls back to the in-progress default
            // and the merge-poller sweep picks it up on its next pass.
            let probe = self.merge_probe.clone();
            let work_db = self.work_db.clone();
            let publisher = self.publisher.clone();
            let candidate = PendingMergeCheck {
                work_item_id: work_item_id.clone(),
                product_id: product_id.clone(),
                pr_url: pr_url.clone(),
            };
            tokio::spawn(async move {
                match probe.probe(&candidate.pr_url).await {
                    Ok(lifecycle_probe) => {
                        update_pr_poll_state(&work_db, publisher.as_ref(), &candidate, &lifecycle_probe, None).await;
                    }
                    Err(err) => {
                        tracing::debug!(
                            work_item_id = %candidate.work_item_id,
                            ?err,
                            "pr completion: on-transition CI pre-fetch failed; \
                             merge poller will retry on next sweep",
                        );
                    }
                }
            });
            StopOutcome::PrDetected { pr_url }
        }
    }

    /// True when the live-state registry reports [`boss_protocol::WorkerActivity::Working`]
    /// for `execution_id`'s slot right now — i.e. the worker is mid-turn,
    /// not between turns. `WaitingForInput` is deliberately treated as
    /// parked rather than mid-turn: it can represent a pending notification
    /// after Stop, so treating it as active could retain a genuinely parked
    /// worker for the full staged-recheck horizon. Returns `false` when no
    /// registry is wired (unit tests) or the run has no live slot: an
    /// unobservable execution is never reported as mid-turn, so this
    /// undercounts rather than fabricating reaps.
    pub(super) fn observed_mid_turn(&self, execution_id: &str) -> bool {
        use boss_protocol::WorkerActivity;

        self.live_worker_states
            .as_ref()
            .and_then(|registry| registry.activity_for_run(execution_id))
            .is_some_and(|activity| activity == WorkerActivity::Working)
    }

    /// Whether a mid-turn PR finalization must defer. Every source is
    /// bounded so a wedged-but-alive worker (`WorkerActivity::Working` never
    /// decays via `downgrade_stale_activity`, and `stale_worker_sweep` skips
    /// slots with a tool in flight) cannot leave the execution active forever.
    ///
    /// - Staged recheck measures the horizon from
    ///   [`crate::pr_url_capture::StagedPrUrlEntry::staged_at`].
    /// - Every other source (detector, `stop_satisfied_clean`, …) measures
    ///   the same [`Self::staged_pr_mid_turn_defer_secs`] horizon from
    ///   `LiveWorkerState::last_event_at`, falling back to the execution's
    ///   `started_at`. Once the horizon expires, finalization proceeds and
    ///   [`Self::record_mid_turn_reap`] fires.
    fn should_defer_mid_turn_pr_completion(&self, execution_id: &str, source: &str) -> bool {
        if source == "pr_recheck_staged" {
            return self.should_defer_staged_pr_recheck(execution_id);
        }
        self.should_defer_mid_turn_within_activity_horizon(execution_id)
    }

    /// True while the mid-turn activity anchor is still inside the shared
    /// deferral horizon. Anchor preference: live-state `last_event_at`, then
    /// execution `started_at`. Missing/unparseable anchors fail open (do not
    /// defer) so terminalization cannot hang without a clock.
    fn should_defer_mid_turn_within_activity_horizon(&self, execution_id: &str) -> bool {
        let horizon_secs = self.staged_pr_mid_turn_defer_secs.max(0);
        if horizon_secs == 0 {
            return false;
        }
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let anchor_epoch = self
            .live_worker_states
            .as_ref()
            .and_then(|registry| registry.last_event_at_for_run(execution_id))
            .and_then(|ts| boss_engine_utils::iso8601::parse_iso8601_to_epoch(&ts))
            .or_else(|| {
                self.work_db.get_execution(execution_id).ok().and_then(|execution| {
                    execution
                        .started_at
                        .as_deref()
                        .and_then(boss_engine_utils::iso8601::parse_iso8601_to_epoch)
                })
            });
        match anchor_epoch {
            Some(epoch) => now.saturating_sub(epoch) < horizon_secs,
            None => false,
        }
    }

    /// Fetch the head that is about to be recorded on a PR-completion
    /// teardown. Forensic only: parse failures and `gh` errors log at WARN
    /// and return `None` so terminalization still proceeds.
    async fn fetch_pr_head_after(&self, execution: &crate::work::WorkExecution, pr_url: &str) -> Option<String> {
        let repo_slug = match parse_repo_slug(&execution.repo_remote_url) {
            Ok(repo_slug) => repo_slug,
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    pr_url,
                    ?err,
                    "pr completion: cannot parse repo slug for pr_head_after; recording None",
                );
                return None;
            }
        };
        let Some(pr_number) = pr_number_from_url(pr_url) else {
            tracing::warn!(
                execution_id = %execution.id,
                pr_url,
                "pr completion: cannot parse PR number for pr_head_after; recording None",
            );
            return None;
        };
        match self
            .branch_verifier
            .fetch_pr_head_oid_fresh(&repo_slug, pr_number)
            .await
        {
            Ok(head) => Some(head),
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    pr_url,
                    ?err,
                    "pr completion: fresh pr_head_after read failed; recording None",
                );
                None
            }
        }
    }

    /// Record when a mid-turn finalization proceeds past its deferral horizon:
    /// bump the aggregate and per-source counters and, for revision
    /// implementations, file a sticky attention item on the execution. This
    /// is the explicit escape hatch for a wedged mid-tool worker that never
    /// reaches Stop — not a successful clean-boundary teardown.
    ///
    /// Deliberately does not touch [`crate::live_worker_state::LiveWorkerStateRegistry::release_slot`]'s
    /// own `activity` log line — that remains the forensic record this
    /// only adds a first-class signal alongside, per the postmortem's
    /// explicit forbidden-workaround list.
    async fn record_mid_turn_reap(&self, execution: &crate::work::WorkExecution, source: &'static str) {
        MID_TURN_REAP_TOTAL.inc(&self.metrics);
        self.metrics.counter_inc_by_dynamic(
            &format!("completion.mid_turn_reap.{source}.count"),
            "PR-completion finalizations on this source that observed the producing execution \
             mid-turn (activity=working) rather than idle at finalize time.",
            1,
        );
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            source,
            "pr completion: finalizing via {source} while producing execution is mid-turn \
             (activity=working) — worker will be reaped before its remaining turn runs",
        );
        if execution.kind != ExecutionKind::RevisionImplementation {
            return;
        }
        let body = format!(
            "The engine finalized this execution's PR via `{source}` while the worker's live \
             activity was still `working` — it had not reached its own Stop boundary. The pane \
             is being torn down immediately after this, so any remaining steps in the worker's \
             current turn (unpushed commits, PR description/comment updates, etc.) will not run.\n\n\
             See tools/boss/docs/postmortems/incident-004-live-revision-workers-reaped-mid-turn.md \
             for the incident this class of reap comes from."
        );
        if let Err(err) = self
            .file_execution_attention(
                execution,
                MID_TURN_REAP_ATTENTION_KIND,
                "Worker reaped mid-turn during PR-completion finalize",
                body,
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "mid-turn reap: failed to file attention item",
            );
        }
    }

    /// Push an `AttentionCreated` event per newly-created member on the
    /// owning product's work-tree topic so the Notifications window and the
    /// design-doc viewer live-update (mirrors the `CreateAttention` RPC
    /// handler). No-op for an empty `created` set.
    pub(super) async fn publish_attentions_created(&self, group: &AttentionGroup, created: &[Attention]) {
        for attention in created {
            self.publisher
                .publish_frontend_event_on_product(
                    &group.product_id,
                    FrontendEvent::AttentionCreated {
                        attention: attention.clone(),
                        group: group.clone(),
                    },
                )
                .await;
        }
    }
}
