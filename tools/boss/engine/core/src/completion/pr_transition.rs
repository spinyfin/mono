//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

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
        let merged = matches!(target, WorkerPrCompletionTarget::Done);

        // For reviewer-triggering executions with a fresh
        // (non-merged) PR, try to enqueue an independent reviewer pass
        // instead of immediately advancing to human Review.
        // This also checks the cycle bound first — if review_cycle has
        // already reached max_review_cycles, skip the reviewer and proceed
        // to InReview with a sticky attention item for the human.
        // If the pr_review execution cannot be created (DB error), fall back
        // to the normal InReview path so the task is never left stuck.
        let enqueued_reviewer = if !merged && matches!(target, WorkerPrCompletionTarget::InReview) {
            match self.work_db.get_execution(execution_id) {
                Ok(ref producing)
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
                    let noop_skip_reason = self
                        .check_noop_skip(&pr_url, producing, review_cycle, last_reviewed_sha.as_deref())
                        .await;

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
                            match self
                                .work_db
                                .create_pr_review_execution_dedup(&producing.work_item_id, &producing.repo_remote_url)
                            {
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

        let completion = match self
            .work_db
            .record_worker_pr_completion(execution_id, &pr_url, None, effective_target)
        {
            Ok(Some(completion)) => completion,
            Ok(None) => return StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(execution_id, source, ?err, "pr completion: failed to record");
                return StopOutcome::DbError;
            }
        };
        // Clear the staged URL now that the DB write succeeded.
        // Deliberately ordered after `record_worker_pr_completion` so
        // a failed DB write leaves the cache intact and the next
        // merge-poller sweep can retry with the same staged URL.
        self.staged_pr_urls.forget(execution_id);
        // The worker contributed a PR — reset any accumulated nudge
        // count so a later unrelated nudge cycle starts clean.
        self.nudge_breaker.forget(execution_id);
        self.build_wait_tracker.forget(execution_id);
        if let Some(lease_id) = completion.released_lease_id.as_deref()
            && let Err(err) = self.cube_client.release_workspace(lease_id).await
        {
            tracing::error!(
                execution_id,
                source,
                lease_id,
                ?err,
                "pr completion: cube release failed"
            );
        }
        self.pane_releaser.release_pane(execution_id).await;
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
        // Doc-link auto-population. The doc-link card affordance is driven
        // by a resolved doc pointer, and which pointer depends on the item:
        //  - design tasks WITH a project -> the PROJECT's `design_doc_*`
        //    pointer (`design_detector::on_design_pr_*`), surfaced on
        //    design cards.
        //  - project-less docs-backed items (investigations, and any
        //    project-less design task) -> the TASK's own `doc_*` pointer
        //    (`design_detector::on_task_doc_pr_*`), surfaced on
        //    investigation cards.
        // The routing decision is logged ABOVE the per-branch dispatch so a
        // skip/proceed is ALWAYS visible without entering a gated block.
        // This closes the historical diagnostic blind spot: investigations
        // failed BOTH the old `kind == Design` and `project_id.is_some()`
        // gates, so every diagnostic that lived inside the block stayed
        // silent for them. Detector errors are logged inside the detector —
        // they must not surface here because they'd mask the successful PR
        // transition.
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
            let uses_task_doc = design_detector::task_uses_per_task_doc(&task.kind, task.project_id.is_none());
            let decision = if produces_project_design {
                "project-design-doc"
            } else if uses_task_doc {
                "per-task-doc"
            } else {
                "skipped: kind produces no doc"
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

            if uses_task_doc {
                if merged {
                    // Worker merged directly during its session; the detector
                    // fetches base_ref_name from the PR (unknown here).
                    design_detector::on_task_doc_pr_merged(&self.work_db, &task.id, &task.product_id, &pr_url, None)
                        .await;
                } else {
                    design_detector::on_task_doc_pr_detected(&self.work_db, &task.id, &task.product_id, &pr_url).await;
                }
            }
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
        // out-of-scope follow-on work. PRIMARY: the engine-owned
        // structured-output artifact (a `FollowupEntry` JSON array). FALLBACK:
        // a `FOLLOWUPS:` block scraped from the transcript tail. A no-op (no
        // artifact / no transcript / no block) when absent; idempotent across
        // re-runs via the store's content dedup.
        //
        // Skipped for `design_postmortem`: its worker prompt never asks for
        // a `FollowupEntry`-shaped artifact (only the stronger
        // `postmortem_followups` schema above), so this would only ever
        // find nothing (or, if reusing the same artifact path, fail to
        // parse it as `FollowupEntry` and log spurious noise).
        if !is_design_postmortem {
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
                    self.publish_attentions_created(&group, &created).await;
                }
            }
        }
        // Reap the engine-owned followups artifact regardless of outcome (it
        // lives in the system temp dir, but delete eagerly rather than waiting
        // on OS reaping).
        crate::structured_output::clear(&self.structured_output_dir, execution_id);

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
                        update_pr_poll_state(&work_db, publisher.as_ref(), &candidate, &lifecycle_probe).await;
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
