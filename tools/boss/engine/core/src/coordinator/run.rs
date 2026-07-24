//! Running an execution to completion: spawn, conflict-diagnosis capture,
//! worker release, and run-completion bookkeeping. Part of the `coordinator`
//! module split; see [`super`] for the struct and shared types.
use super::*;

impl ExecutionCoordinator {
    // `change` is `None` for `pr_review` executions that checked out the PR
    // head directly; `Some` for all other executions that created a jj change.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_execution(
        self: Arc<Self>,
        execution: WorkExecution,
        run: WorkRun,
        work_item: WorkItem,
        worker_id: String,
        lease: CubeWorkspaceLease,
        change: Option<CubeChangeHandle>,
        adapter: Arc<dyn HostAdapter>,
    ) {
        // Keep the cube lease alive for blocking runners (in-process test
        // fakes, ACP-style runners). For pane-spawn (the production path),
        // spawn_worker returns immediately and this guard is dropped below
        // without ever firing — pane workers are covered instead by the
        // engine-wide periodic sweep in `crate::cube_lease_heartbeat`.
        // See the HeartbeatGuard doc comment for the full coverage split.
        let heartbeat = HeartbeatGuard::spawn(
            Arc::clone(&adapter),
            lease.lease_id.clone(),
            execution.id.clone(),
            run.id.clone(),
            worker_id.clone(),
        );

        // Pre-spawn: collect the merge-tree diagnosis for revision_implementation
        // executions with merge-conflict provenance so compose_revision_directive
        // injects it into the worker prompt. No-op for other provenance.
        if execution.kind == ExecutionKind::RevisionImplementation {
            self.collect_revision_conflict_diagnosis_pre_spawn(&execution, &work_item, &lease)
                .await;
        }

        let run_outcome = adapter
            .spawn_worker(
                &worker_id,
                &execution,
                &work_item,
                lease.workspace_path.as_path(),
                change.as_ref().map(|c| c.change_id.as_str()),
            )
            .await;
        drop(heartbeat);

        // Pane-spawn runs hand the slot to a live libghostty pane; the
        // WorkerPool slot must remain claimed until that pane is torn
        // down by `ServerState::release_worker_pane` (completion, force
        // release, or engine shutdown). Releasing it here would let a
        // concurrent dispatch re-claim the same slot while the pane
        // still owns it, and the app would reject `SpawnWorkerPane`
        // with `SlotBusy`. Non-pane runs (test fakes, future
        // ACP-style runners) leave `slot_id = None` and still need
        // the inline release.
        let defer_pool_slot_release = matches!(
            run_outcome.as_ref(),
            Ok(outcome) if outcome.slot_id.is_some()
        );

        // Set inside the `Err(err)` arm below once a `SlotBusy` pane-spawn
        // rejection has actually been recorded as a terminal `failed`
        // execution (i.e. `finish_execution_run` itself succeeded).
        // Hoisted above the match so the tail of this function (worker-pool
        // release decision) can see it after `run_outcome` is consumed.
        // Deliberately left `false` if `finish_execution_run` errors: in
        // that rare double-fault the execution may still be non-terminal
        // in the DB, and holding the slot for a row `pool_claim_sweep`
        // will never consider terminal would leak it forever — releasing
        // normally is the safe fallback there.
        let mut hold_slot_busy = false;

        match run_outcome {
            // Mid-spawn cancel (T981): the worker was cancelled while it
            // was still spawning. The runner has already reaped the
            // just-spawned pane; our job is to release the cube lease the
            // cancel path deliberately left held (so a still-occupied
            // workspace was never handed back to cube) and to skip the
            // normal completion recording — the row is already
            // `cancelled`, so `finish_execution_run` would reject it.
            Ok(outcome) if outcome.wait_state == RunWaitState::CancelledDuringSpawn => {
                // Claim ownership of the lease atomically before calling
                // cube, mirroring `force_release`: whichever path clears
                // the workspace columns first owns the release, so a
                // concurrent `force_release` and this branch can't issue
                // a duplicate cube release against the same lease.
                let released = match self.work_db.clear_execution_workspace(&execution.id) {
                    Ok(Some(lease_id)) => match adapter.release_workspace(&lease_id).await {
                        Ok(()) => true,
                        Err(err) => {
                            tracing::error!(
                                ?err,
                                execution_id = %execution.id,
                                run_id = %run.id,
                                lease_id = %lease_id,
                                "failed to release deferred lease after mid-spawn cancel",
                            );
                            false
                        }
                    },
                    // Already cleared by a racing force_release that saw
                    // the slot mapped and reaped + released itself.
                    Ok(None) => false,
                    Err(err) => {
                        tracing::error!(
                            ?err,
                            execution_id = %execution.id,
                            "failed to clear workspace columns after mid-spawn cancel",
                        );
                        false
                    }
                };
                tracing::warn!(
                    execution_id = %execution.id,
                    run_id = %run.id,
                    worker_id = %worker_id,
                    released_workspace = released,
                    "reconciled mid-spawn cancel: worker pane reaped, deferred lease released",
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::PaneSpawned, DispatchOutcome::Skipped, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(&worker_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_details(serde_json::json!({
                                "run_id": run.id,
                                "cancelled_during_spawn": true,
                                "released_workspace": released,
                            })),
                    )
                    .await;
                // The pane was already torn down by the runner (which
                // also released the pool slot), and `defer_pool_slot_release`
                // is false for this outcome (slot_id = None), so the tail
                // below frees the pool slot idempotently.
            }
            Ok(outcome) => {
                // Capture the resolved spawn knobs (effort level,
                // claude effort value, model) before `outcome` moves
                // into `record_run_completion` — they ride along on
                // the `pane_spawned` dispatch event below so a
                // diagnose verb can answer "what did the worker
                // actually launch with" without scraping process
                // argv. `None` from test fake runners that don't go
                // through `effort::resolve_spawn_config`.
                let spawn_config_for_event = outcome.spawn_config.clone();
                // If the runner allocated a real pane slot for this
                // run, stamp it onto the run record's agent_id so
                // `bossctl agents list` and related views show one
                // entry per active pane. Test runners that don't
                // allocate a pane leave slot_id as None and the
                // worker-pool placeholder (worker_id) stays as the
                // agent_id.
                let run = if let Some(slot_id) = outcome.slot_id {
                    let agent_id = worker_id_for_slot(slot_id);
                    match self.work_db.set_run_agent_id(&run.id, &agent_id) {
                        Ok(updated) => updated,
                        Err(err) => {
                            tracing::error!(
                                ?err,
                                execution_id = %execution.id,
                                run_id = %run.id,
                                slot_id,
                                "failed to stamp pane slot onto run record"
                            );
                            run
                        }
                    }
                } else {
                    run
                };
                if let Err(err) = self
                    .record_run_completion(&execution, &run, &lease, &worker_id, outcome, &adapter)
                    .await
                {
                    tracing::error!(
                        ?err,
                        execution_id = %execution.id,
                        run_id = %run.id,
                        worker_id = %worker_id,
                        "failed to record execution completion"
                    );
                }
                // Successful spawn → emit a structured `pane_spawned`
                // event so consumers can pair it with the
                // `cube_workspace_leased` event that preceded it and
                // see the full timeline. The `spawn_config` details
                // carry the effort + model tuple the dispatcher just
                // resolved — design §Q2 calls this out explicitly so
                // `bossctl dispatch diagnose <exec-id>` can answer
                // "which model / effort did this worker actually
                // launch with."
                let mut details = serde_json::json!({
                    "run_id": run.id,
                    "slot_id": slot_id_from_worker_id(&worker_id),
                    "page": slot_id_from_worker_id(&worker_id).and_then(worker_page_label),
                });
                if let Some(spawn) = spawn_config_for_event {
                    details["spawn_config"] = serde_json::json!({
                        "effort_level": spawn.effort_level.map(|level| level.as_str()),
                        "claude_effort": spawn.claude_effort,
                        "model": spawn.model,
                        "prompt_addendum_applied": spawn.prompt_addendum.is_some(),
                    });
                }
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::PaneSpawned, DispatchOutcome::Ok, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(&worker_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_details(details),
                    )
                    .await;
            }
            Err(err) => {
                let released = match adapter.release_workspace(&lease.lease_id).await {
                    Ok(()) => true,
                    Err(release_err) => {
                        tracing::error!(
                            ?release_err,
                            execution_id = %execution.id,
                            run_id = %run.id,
                            lease_id = %lease.lease_id,
                            "failed to release workspace after run failure"
                        );
                        false
                    }
                };
                let error_text = err.to_string();

                // A `SlotBusy` app rejection means the engine and the app
                // disagree about this specific slot's occupancy — the app
                // itself documents this as "the engine should reconcile
                // rather than retry blindly" (see
                // `WorkersWorkspaceModel.spawnWorkerPane`'s doc comment).
                // It is an engine/app desync, not a genuine task or
                // automation failure, so it is handled differently below:
                // the work stays queued instead of bouncing to a terminal
                // state, and the offending slot is held out of rotation
                // (rather than freed) so the very next dispatch pass
                // doesn't just re-select the same bad slot and repeat the
                // rejection — see the tail of this function.
                let is_slot_busy = slot_busy_occupant(&err).is_some();

                // Historical silent-release path: a pane-spawn
                // failure (libghostty IPC drop, slot busy, prompt
                // composition error) inside `run_execution` marked
                // the run `failed` and released the lease without
                // raising anything the operator could see. Attach a
                // `WorkAttentionItem` to this run so the failure
                // turns up in the kanban "Attention" lane and via
                // `ListAttentionItems`. The structured event below
                // gives tooling a parallel signal.
                let err_detail = format!("{err:#}");
                let attention = Some(CreateAttentionItemInput {
                    execution_id: Some(execution.id.clone()),
                    work_item_id: None,
                    kind: "pane_spawn_failed".to_owned(),
                    status: None,
                    title: "Worker pane failed to spawn".to_owned(),
                    body_markdown: format!(
                        "Execution `{exec_id}` leased workspace `{ws}` but the worker pane never came up.\n\n\
                         **Error:** {err_detail}\n\n\
                         The lease was {release_state}. Inspect \
                         `dispatch-events/executions/{exec_id}/dispatch.jsonl` for the full stage timeline.",
                        exec_id = execution.id,
                        ws = lease.workspace_id,
                        release_state = if released {
                            "released back to cube"
                        } else {
                            "still held by the engine (release failed — see the engine log)"
                        },
                    ),
                    resolved_at: None,
                });

                match self.work_db.finish_execution_run(
                    FinishExecutionRunInput::builder()
                        .execution_id(&execution.id)
                        .run_id(&run.id)
                        .execution_status(ExecutionStatus::Failed)
                        .run_status("failed")
                        .error_text(error_text.as_str())
                        .clear_workspace_lease(released)
                        .maybe_attention(attention)
                        .build(),
                ) {
                    Ok((execution, _run, _)) => {
                        // The execution is now durably `failed` in the DB —
                        // safe to have `pool_claim_sweep` own reclaiming this
                        // slot instead of releasing it immediately (see the
                        // `hold_slot_busy` declaration above and the tail of
                        // this function).
                        hold_slot_busy = is_slot_busy;
                        tracing::warn!(
                            execution_id = %execution.id,
                            run_id = %run.id,
                            worker_id = %worker_id,
                            error = %err,
                            released_workspace = released,
                            "execution run failed"
                        );
                        let mut error_details = serde_json::json!({
                            "run_id": run.id,
                            "released_workspace": released,
                            "slot_id": slot_id_from_worker_id(&worker_id),
                            "page": slot_id_from_worker_id(&worker_id).and_then(worker_page_label),
                        });
                        // A `SlotBusy` spawn rejection means the engine and
                        // the app disagree about slot occupancy — the
                        // engine already knew which slot it requested
                        // (`worker_id` above), but not which pane the app
                        // reports as squatting it. Surface both explicitly
                        // so `dispatch.jsonl` is self-diagnosing instead of
                        // requiring a coordinator to cross-reference the
                        // husk pane by hand.
                        if let Some(occupying_run_id) = slot_busy_occupant(&err) {
                            error_details["slot_busy"] = serde_json::json!({
                                "slot_id": slot_id_from_worker_id(&worker_id),
                                "occupying_run_id": occupying_run_id,
                            });
                        }
                        self.dispatch_events
                            .emit(
                                DispatchEvent::new(Stage::PaneSpawned, DispatchOutcome::Error, &execution.id)
                                    .with_work_item(&execution.work_item_id)
                                    .with_worker(&worker_id)
                                    .with_cube_lease(&lease.lease_id)
                                    .with_cube_workspace(&lease.workspace_id)
                                    .with_error(&err)
                                    .with_details(error_details),
                            )
                            .await;
                        // Clear the card out of `active`. The run is
                        // already recorded `failed` and the workspace
                        // released, but the work item itself stays
                        // `active` — so the kanban keeps the green
                        // "Doing" card and the orphan-active sweep
                        // re-dispatches the same doomed spawn every
                        // cycle. Demote it back to To-Do so the failure
                        // (already surfaced as a `pane_spawn_failed`
                        // attention item) is recoverable rather than a
                        // silent green-flicker strand.
                        //
                        // Exception: PrReview spawn failures are engine
                        // infrastructure bugs (e.g. slot-range mismatch),
                        // not task regressions. Demoting the work item
                        // here would silently move a reviewed PR back to
                        // To-Do, erasing the review context. Leave the
                        // task in place — the attention item already
                        // surfaces the failure for the operator.
                        //
                        // Exception: a `SlotBusy` rejection is likewise an
                        // engine-side infrastructure issue (see `is_slot_busy`
                        // above), not a real dispatch failure of the task
                        // itself — demoting to To-Do would require a human to
                        // notice and manually re-drag the card. Leaving the
                        // item `active` lets the tail of this function's
                        // rescan (`rescan_active_dispatch_after_release`)
                        // queue a fresh execution automatically, so the item
                        // stays in Doing and dispatches onto the next free
                        // slot exactly like a plain pool-exhaustion wait.
                        if execution.kind != ExecutionKind::PrReview && !is_slot_busy {
                            match self.work_db.demote_active_work_item_to_todo(&execution.work_item_id) {
                                Ok(true) => tracing::info!(
                                    execution_id = %execution.id,
                                    work_item_id = %execution.work_item_id,
                                    "demoted work item to todo after pane-spawn failure",
                                ),
                                Ok(false) => {}
                                Err(demote_err) => tracing::error!(
                                    ?demote_err,
                                    work_item_id = %execution.work_item_id,
                                    "failed to demote work item out of active after pane-spawn failure",
                                ),
                            }
                        } else {
                            tracing::info!(
                                execution_id = %execution.id,
                                work_item_id = %execution.work_item_id,
                                is_slot_busy,
                                "skipping demote for pr_review or slot-busy spawn failure — engine infrastructure issue, not a task regression",
                            );
                        }
                        self.publisher
                            .publish(
                                &execution.id,
                                &execution.work_item_id,
                                execution.status.as_str(),
                                "execution_run_failed",
                            )
                            .await;
                        if let Ok(item) = self.work_db.get_work_item(&execution.work_item_id) {
                            self.publisher
                                .publish_work_item_changed(
                                    item.product_id(),
                                    &execution.work_item_id,
                                    "execution_run_failed",
                                )
                                .await;
                        }
                        // A pane-spawn failure is terminal — the execution is
                        // now `failed` and the workspace has been released. If
                        // this was an automation triage run, the matching
                        // `automation_runs` row is still sitting at the
                        // pessimistic `failed_will_retry` that the scheduler
                        // stamped when it dispatched the triage execution.
                        //
                        // A genuine spawn failure (bad config, IPC down, …)
                        // flips it to `failed_gave_up` so the Automations tab
                        // shows an accurate terminal state instead of implying
                        // a self-healing retry is pending — it will not
                        // recover on its own. A `SlotBusy` rejection is the
                        // opposite: it self-heals as soon as the offending
                        // slot is reconciled (see the tail of this function),
                        // so instead of giving up we fire a fresh triage
                        // execution immediately — same automation, same repo
                        // — and re-point this occurrence's `automation_runs`
                        // row at it, mirroring `EngineTriageDispatcher::fire`
                        // rather than waiting for the automation's next
                        // scheduled occurrence.
                        if execution.kind == ExecutionKind::AutomationTriage {
                            if is_slot_busy {
                                match self.work_db.create_automation_triage_execution(
                                    &execution.work_item_id,
                                    &execution.repo_remote_url,
                                ) {
                                    Ok(retry_execution) => {
                                        if let Err(err) =
                                            self.work_db.requeue_automation_run_after_transient_spawn_failure(
                                                &execution.id,
                                                &retry_execution.id,
                                                &format!("slot busy at spawn; requeued as {}", retry_execution.id),
                                            )
                                        {
                                            tracing::warn!(
                                                execution_id = %execution.id,
                                                retry_execution_id = %retry_execution.id,
                                                ?err,
                                                "failed to re-point automation run at retry execution after slot-busy spawn failure",
                                            );
                                        }
                                        tracing::info!(
                                            execution_id = %execution.id,
                                            retry_execution_id = %retry_execution.id,
                                            "requeued automation triage after slot-busy pane-spawn failure",
                                        );
                                    }
                                    Err(create_err) => {
                                        tracing::error!(
                                            execution_id = %execution.id,
                                            ?create_err,
                                            "failed to create retry triage execution after slot-busy spawn failure; giving up",
                                        );
                                        if let Err(finalize_err) = self.work_db.finalize_automation_triage_run(
                                            &execution.id,
                                            boss_protocol::AUTOMATION_OUTCOME_FAILED_GAVE_UP,
                                            None,
                                            Some(&format!(
                                                "pane spawn failed: {error_text}; retry creation also failed: {create_err:#}"
                                            )),
                                        ) {
                                            tracing::warn!(
                                                execution_id = %execution.id,
                                                ?finalize_err,
                                                "failed to mark automation run failed_gave_up after retry-creation failure",
                                            );
                                        }
                                    }
                                }
                            } else if let Err(finalize_err) = self.work_db.finalize_automation_triage_run(
                                &execution.id,
                                boss_protocol::AUTOMATION_OUTCOME_FAILED_GAVE_UP,
                                None,
                                Some(&format!("pane spawn failed: {error_text}")),
                            ) {
                                tracing::warn!(
                                    execution_id = %execution.id,
                                    ?finalize_err,
                                    "failed to mark automation run failed_gave_up after pane-spawn failure",
                                );
                            }
                        }
                    }
                    Err(record_err) => {
                        tracing::error!(
                            ?record_err,
                            execution_id = %execution.id,
                            run_id = %run.id,
                            worker_id = %worker_id,
                            "failed to record execution run failure"
                        );
                    }
                }
            }
        }

        if !defer_pool_slot_release {
            if hold_slot_busy {
                // Do NOT hand the slot back to `select_claim_index` — the
                // app just told us it's still hosting a real pane there, so
                // freeing it now would let the very next dispatch pass
                // re-select the same slot and repeat the rejection (an
                // effective blind retry loop). Instead leave the claim
                // attributed to this (now terminal) execution: the existing
                // pool-claim reconciler (`pool_claim_sweep::run_one_pass`)
                // already frees exactly this shape of stuck claim — terminal
                // execution, no live worker pane backing it — once its
                // `LEAK_GRACE_SECS` grace period has passed, by which point
                // the app side has normally torn the stray pane down itself
                // or the husk-pane sweep has retired it. Still rescan + kick
                // so OTHER free slots pick up the work this failure just
                // requeued.
                self.rescan_active_dispatch_after_release();
                // `rescan_active_dispatch` only requeues items with
                // `autostart = 1` — but `start_execution_run_on_host`
                // consumes (clears) `autostart` the moment a run first
                // starts (deliberate single-shot semantics: an item that
                // already got its automatic shot doesn't respawn forever
                // unattended). A `SlotBusy` desync isn't that case at
                // all — it's the engine deciding, on the work item's
                // behalf, that THIS dispatch attempt needs to be retried
                // because it never really ran on the merits — so route
                // around the autostart gate entirely for the ordinary
                // task/chore/revision family by requesting a fresh
                // execution directly. Excluded: `PrReview` (needs the
                // dedicated re-fire path, not a plain `request_execution`,
                // to land the right kind) and `AutomationTriage` /
                // `AnswerAgent` (synthetic work items with no `tasks` row
                // — `AutomationTriage` already got its own fresh execution
                // above; `AnswerAgent` is unhandled here, matching its
                // pre-existing scope).
                if !matches!(
                    execution.kind,
                    ExecutionKind::PrReview | ExecutionKind::AutomationTriage | ExecutionKind::AnswerAgent
                ) && let Err(err) = self.work_db.request_execution(
                    boss_protocol::RequestExecutionInput::builder()
                        .work_item_id(execution.work_item_id.clone())
                        .build(),
                ) {
                    tracing::warn!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        ?err,
                        "failed to queue a fresh execution after slot-busy pane-spawn failure",
                    );
                }
                self.kick();
            } else {
                self.release_worker_and_kick(&worker_id, Some(lease.workspace_id.as_str()))
                    .await;
            }
        }
    }

    /// Phase 3 cutover: for revision_implementation executions with merge-conflict
    /// provenance, resolve the linked `conflict_resolutions` row (via
    /// `created_via = "merge-conflict:<crz_id>"`) and collect its diagnosis:
    /// resolve the `conflict_resolutions` row a merge-conflict revision was
    /// spawned from (via `created_via = "merge-conflict:<crz_id>"`) and
    /// collect its diagnosis. No-op when the revision's provenance is not a
    /// merge conflict (e.g. operator/CI-fix revisions), or when a diagnosis
    /// is already stored (a respawn).
    async fn collect_revision_conflict_diagnosis_pre_spawn(
        &self,
        execution: &WorkExecution,
        work_item: &WorkItem,
        lease: &CubeWorkspaceLease,
    ) {
        let created_via = match work_item {
            WorkItem::Task(task) | WorkItem::Chore(task) => task.created_via.as_str(),
            _ => return,
        };
        let Some(crz_id) = created_via.strip_prefix(boss_protocol::CREATED_VIA_MERGE_CONFLICT_PREFIX) else {
            return;
        };
        let attempt = match self.work_db.get_conflict_resolution(crz_id) {
            Ok(Some(a)) => a,
            Ok(None) => {
                tracing::debug!(
                    execution_id = %execution.id,
                    crz_id,
                    "collect_conflict_diagnosis: revision's linked attempt row missing; skipping",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    crz_id,
                    ?err,
                    "collect_conflict_diagnosis: failed to look up revision's linked attempt; skipping",
                );
                return;
            }
        };
        if attempt.conflict_diagnosis.is_some() {
            tracing::debug!(
                attempt_id = %attempt.id,
                "collect_conflict_diagnosis: diagnosis already present on linked attempt; skipping",
            );
            return;
        }
        self.collect_conflict_diagnosis_for_attempt(&attempt, lease).await;
    }

    /// Run `conflict_diagnosis::collect` in the leased workspace and persist
    /// the result on `attempt`. Shared by the bespoke `conflict_resolution`
    /// path and the Phase 3 merge-conflict revision path. Best-effort —
    /// failures are logged but never propagate.
    async fn collect_conflict_diagnosis_for_attempt(
        &self,
        attempt: &crate::work::ConflictResolution,
        lease: &CubeWorkspaceLease,
    ) {
        let base_sha = attempt.base_sha_at_trigger.as_deref().unwrap_or("");
        let head_sha = attempt.head_sha_before.as_deref().unwrap_or("");
        if base_sha.is_empty() || head_sha.is_empty() {
            tracing::debug!(
                attempt_id = %attempt.id,
                "collect_conflict_diagnosis: missing base/head sha; skipping",
            );
            return;
        }

        let diagnosis = match conflict_diagnosis::collect(&lease.workspace_path, base_sha, head_sha).await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    workspace_path = %lease.workspace_path.display(),
                    ?err,
                    "collect_conflict_diagnosis: git spawn failed; using errored diagnosis",
                );
                conflict_diagnosis::ConflictDiagnosis::errored(base_sha, head_sha, format!("git spawn failed: {err}"))
            }
        };

        let json = match serde_json::to_string(&diagnosis) {
            Ok(j) => j,
            Err(err) => {
                tracing::warn!(attempt_id = %attempt.id, ?err, "collect_conflict_diagnosis: failed to serialize diagnosis");
                return;
            }
        };

        match self.work_db.set_conflict_resolution_diagnosis(&attempt.id, &json) {
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?err,
                    "collect_conflict_diagnosis: failed to persist diagnosis; continuing without it",
                );
            }
            Ok(updated) => {
                tracing::debug!(
                    attempt_id = %attempt.id,
                    conflicted_files = diagnosis.files.len(),
                    "collect_conflict_diagnosis: diagnosis persisted",
                );
                let counted = updated.and_then(|row| row.conflict_class.clone().map(|class| (row.product_id, class)));
                if let Some((product_id, class)) = counted {
                    crate::merge_poller::record_conflict_class_counter(&self.metrics, &product_id, &class);
                }
            }
        }
    }

    /// Release `worker_id` back to the pool, then rescan + kick to
    /// pick up newly-eligible work. Used at the tail of non-pane
    /// `run_execution` calls and from [`ServerState::release_worker_pane`]
    /// for the deferred pane-spawn case — the engine and the app must
    /// agree on which slots are busy, so the WorkerPool free signal is
    /// paired with the libghostty pane teardown rather than firing as
    /// soon as the spawn RPC returns.
    pub async fn release_worker_and_kick(self: &Arc<Self>, worker_id: &str, last_workspace_id: Option<&str>) {
        self.pool_for_worker_id(worker_id)
            .release_worker(worker_id, last_workspace_id)
            .await;
        self.rescan_active_dispatch_after_release();
        self.kick();
    }

    /// Compare-and-release variant of [`Self::release_worker_and_kick`]
    /// for the pool-claim reconciler: free `worker_id` only if it is
    /// still claimed by exactly `execution_id`, then rescan + kick if it
    /// was actually freed. Returns whether the slot was released.
    ///
    /// The execution-id guard makes this safe against the re-claim race
    /// the reconciler is exposed to (snapshot a leaked claim, release it
    /// later) — see [`WorkerPool::release_worker_if_execution`]. The
    /// rescan + kick only fire on a real release so a no-op (already
    /// freed, or re-claimed by a live execution) doesn't churn the
    /// scheduler.
    pub async fn release_pool_claim_if_execution(self: &Arc<Self>, worker_id: &str, execution_id: &str) -> bool {
        let released = self
            .pool_for_worker_id(worker_id)
            .release_worker_if_execution(worker_id, execution_id, None)
            .await;
        if released {
            self.rescan_active_dispatch_after_release();
            self.kick();
        }
        released
    }

    /// Steady-state rescan of `tasks.status = 'active'` work that
    /// never made it onto a worker. The create-time path already
    /// queues a `ready` execution and `kick()`s the scheduler, but a
    /// chore whose dispatch failed (cube lease error, kanban drag
    /// while the pool was full, worker died after starting) leaves
    /// the kanban card in `active` with a *terminal* (or absent)
    /// execution row — `list_ready_executions` skips it and `kick()`
    /// alone is not enough to reanimate it. Running
    /// [`WorkDb::rescan_active_dispatch`] before each kick fixes
    /// that: items whose latest execution is terminal (or missing)
    /// get a fresh `ready` row, and the scheduler picks them up on
    /// the just-released worker. Errors are logged and swallowed —
    /// the rescan is a best-effort opportunistic sweep, not a hard
    /// invariant.
    fn rescan_active_dispatch_after_release(&self) {
        match self.work_db.rescan_active_dispatch() {
            Ok(redispatched) if !redispatched.is_empty() => {
                tracing::info!(
                    count = redispatched.len(),
                    ids = ?redispatched,
                    "rescanned waiting active work after worker release",
                );
            }
            Ok(_) => {}
            Err(err) => {
                tracing::error!(?err, "active-dispatch rescan failed after worker release; continuing",);
            }
        }
    }

    async fn record_run_completion(
        &self,
        execution: &WorkExecution,
        run: &WorkRun,
        lease: &CubeWorkspaceLease,
        worker_id: &str,
        outcome: RunOutcome,
        adapter: &Arc<dyn HostAdapter>,
    ) -> Result<()> {
        let release_workspace = outcome.wait_state.release_workspace();
        let released = if release_workspace {
            match adapter.release_workspace(&lease.lease_id).await {
                Ok(()) => true,
                Err(err) => {
                    tracing::error!(
                        ?err,
                        execution_id = %execution.id,
                        run_id = %run.id,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after successful run"
                    );
                    false
                }
            }
        } else {
            false
        };

        let attention = outcome.attention.map(|attention| CreateAttentionItemInput {
            execution_id: Some(execution.id.clone()),
            work_item_id: None,
            kind: attention.kind,
            status: None,
            title: attention.title,
            body_markdown: attention.body_markdown,
            resolved_at: None,
        });

        let (execution, run, attention) = self.work_db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&execution.id)
                .run_id(&run.id)
                .execution_status(outcome.wait_state.execution_status())
                .run_status("completed")
                .maybe_result_summary(outcome.result_summary.as_deref())
                .clear_workspace_lease(released)
                .maybe_attention(attention)
                .build(),
        )?;

        // NOTE: for a normal pane spawn this fires ~milliseconds after the
        // pane comes up — the `WorkRun` tracks the engine's dispatch/spawn
        // ACTION (a ~5-8s lifetime), NOT the worker's lifetime. `run_status`
        // going `completed` here while `execution_status` is a LIVE park
        // (`waiting_human` / `running`) is the worker handing off to its
        // pane, not the worker finishing. The `parked_live` field makes that
        // explicit so this line is not misread as "the worker terminalized
        // 3ms after spawn" (the shell_pid-0-window false alarm); a genuine
        // terminalization instead logs "execution terminalized: …".
        tracing::info!(
            execution_id = %execution.id,
            run_id = %run.id,
            worker_id,
            execution_status = %execution.status,
            run_status = %run.status,
            parked_live = execution.status.is_live(),
            attention_created = attention.is_some(),
            released_workspace = released,
            "execution run completed (dispatch/spawn run finalized; execution parks at its wait state — LIVE unless a terminal status is shown)"
        );
        self.publisher
            .publish(
                &execution.id,
                &execution.work_item_id,
                execution.status.as_str(),
                "execution_run_completed",
            )
            .await;
        if let Ok(item) = self.work_db.get_work_item(&execution.work_item_id) {
            self.publisher
                .publish_work_item_changed(item.product_id(), &execution.work_item_id, "execution_run_completed")
                .await;
        }
        Ok(())
    }
}
