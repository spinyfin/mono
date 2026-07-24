//! Per-execution scheduling: work-item resolution, host selection, workspace
//! leasing/recovery, and pre-start failure handling. Part of the `coordinator`
//! module split; see [`super`] for the struct and shared types.
use super::*;

impl ExecutionCoordinator {
    /// Resolve the [`WorkItem`] an execution operates on.
    ///
    /// For a normal execution this is the persisted task/chore/product/
    /// project. An `automation_triage` execution, though, binds to an
    /// `automations.id` — there is no task row for `get_work_item` to find —
    /// so we synthesize an in-memory `Chore` carrying the automation's
    /// product/name/repo. The synthetic item only feeds the task-centric
    /// spawn plumbing (cube task label, change title, product resolution);
    /// the runner branches on `kind` to render the triage preamble and the
    /// completion handler branches on `kind` to run the outcome detector, so
    /// the synthetic fields never drive real task work.
    ///
    /// An `answer_agent` execution (P3b) binds to a `work_comments.id` for
    /// the same reason — see [`crate::work::WorkDb::create_answer_agent_execution`]
    /// — so it gets the same synthetic-item treatment via
    /// [`Self::synthetic_answer_agent_work_item`].
    pub(super) fn resolve_execution_work_item(&self, execution: &WorkExecution) -> Result<WorkItem> {
        if execution.kind == ExecutionKind::AutomationTriage
            && let Some(item) = self.synthetic_triage_work_item(execution)
        {
            return Ok(item);
        }
        if execution.kind == ExecutionKind::AnswerAgent
            && let Some(item) = self.synthetic_answer_agent_work_item(execution)
        {
            return Ok(item);
        }
        self.work_db.get_work_item(&execution.work_item_id)
    }

    /// Build the synthetic `Chore` work item for an `automation_triage`
    /// execution from the bound automation. `None` when the automation row is
    /// gone (deleted mid-flight) — the caller then falls back to the normal
    /// `get_work_item`, which fails cleanly.
    fn synthetic_triage_work_item(&self, execution: &WorkExecution) -> Option<WorkItem> {
        let automation = self.work_db.get_automation(&execution.work_item_id).ok().flatten()?;
        let task = boss_protocol::Task::builder()
            .id(automation.id.clone())
            .product_id(automation.product_id.clone())
            .kind(TaskKind::Chore)
            .name(format!("Automation triage: {}", automation.name))
            .description(automation.standing_instruction.clone())
            .status(TaskStatus::Active)
            .repo_remote_url(execution.repo_remote_url.clone())
            .created_at(automation.created_at.clone())
            .updated_at(automation.updated_at.clone())
            .build();
        Some(WorkItem::Chore(task))
    }

    /// Build the synthetic `Chore` work item for an `answer_agent`
    /// execution (P3b) from the bound comment and its resolved doc owner.
    /// `None` when the comment is gone, or its doc owner no longer resolves
    /// (both are the same "engine state raced under us mid-flight"
    /// tolerance `synthetic_triage_work_item` already applies — the caller
    /// falls back to the normal `get_work_item`, which fails cleanly).
    ///
    /// `product_id` is the doc owner task's product — needed for host
    /// capability resolution ([`Self::select_host_for_execution`]); `name`/
    /// `description` surface the question in cube's task label / change
    /// title. Like the triage synthetic item, these fields only feed spawn
    /// plumbing: the runner (P3b) composes the real answer-agent prompt
    /// separately, and the completion handler branches on `kind` to
    /// finalise the run instead of doing PR detection.
    fn synthetic_answer_agent_work_item(&self, execution: &WorkExecution) -> Option<WorkItem> {
        let comment = self.work_db.get_comment(&execution.work_item_id).ok().flatten()?;
        let doc_owner = self
            .work_db
            .resolve_doc_owner(&comment.artifact_kind, &comment.artifact_id)
            .ok()
            .flatten()?;
        let owner_item = self.work_db.get_work_item(&doc_owner.task_id).ok()?;
        let product_id = owner_item.product_id().to_string();
        let short_quote = if comment.body.chars().count() > 60 {
            format!("{}…", comment.body.chars().take(60).collect::<String>())
        } else {
            comment.body.clone()
        };
        let task = boss_protocol::Task::builder()
            .id(comment.id.clone())
            .product_id(product_id)
            .kind(TaskKind::Chore)
            .name(format!("Answer comment: {short_quote}"))
            .description(comment.body.clone())
            .status(TaskStatus::Active)
            .repo_remote_url(execution.repo_remote_url.clone())
            .created_at(comment.created_at.clone())
            .updated_at(comment.updated_at.clone())
            .build();
        Some(WorkItem::Chore(task))
    }

    /// Pick the host this execution should run on. Honours the pin escape
    /// hatch (`work_executions.pinned_host_id`) and the capability filter,
    /// then ranks the survivors by branch affinity / free slots — see
    /// [`crate::host_scheduling::select_host`].
    ///
    /// The local host is never slot-gated here: the worker pool already
    /// bounded local concurrency before dispatch reached this point, and
    /// `hosts.local.pool_size` defaults to 1, so double-gating on it would
    /// throttle local dispatch to a single concurrent worker. We therefore
    /// report the local slot as always-free (`active_runs = 0`) and let
    /// only remote hosts be gated by their `work_runs` active count.
    ///
    /// Returns the selected [`Host`] or an error describing why nothing was
    /// eligible (consumed by the caller as a recoverable pre-start
    /// failure).
    fn select_host_for_execution(&self, execution: &WorkExecution, work_item: &WorkItem) -> Result<Host> {
        let pinned = self.work_db.execution_pinned_host(&execution.id).unwrap_or_else(|err| {
            tracing::warn!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "host-selection: failed to read pinned host; treating as unpinned",
            );
            None
        });

        // Capability requirements union over the chore + its product +
        // its project. Empty today (no writer yet), which leaves every
        // enabled host capability-eligible — preserving local behaviour.
        let product_id = work_item.product_id().to_string();
        let project_id = work_item_project_id(work_item);
        let mut subject_ids: Vec<&str> = vec![execution.work_item_id.as_str(), product_id.as_str()];
        if let Some(pid) = project_id.as_deref() {
            subject_ids.push(pid);
        }
        let required_capabilities = self
            .work_db
            .required_capabilities_for_subject_ids(&subject_ids)
            .unwrap_or_else(|err| {
                tracing::warn!(
                    execution_id = %execution.id,
                    error = %format!("{err:#}"),
                    "host-selection: failed to read capability requirements; treating as none",
                );
                BTreeSet::new()
            });

        let hosts = self.work_db.list_hosts().context("host-selection: list hosts")?;
        let active = self.work_db.active_runs_per_host().unwrap_or_default();

        let slots: Vec<HostSlot> = hosts
            .iter()
            .map(|host| {
                let capabilities = self
                    .work_db
                    .list_host_capabilities(&host.id)
                    .map(|caps| caps.into_iter().map(|c| c.capability).collect::<BTreeSet<_>>())
                    .unwrap_or_default();
                let active_runs = if host.id == "local" {
                    0
                } else {
                    *active.get(&host.id).unwrap_or(&0)
                };
                HostSlot {
                    host: host.clone(),
                    capabilities,
                    active_runs,
                    // Branch-affinity tiebreaker is deferred (PR4): the
                    // affinity key is the PR branch, which is unset until
                    // the first run pushes. Free-slots-first is the
                    // design's documented v1 fallback for the first run.
                    had_prior_run_on_branch: false,
                }
            })
            .collect();

        let requirements = ChoreRequirements {
            required_capabilities,
            pinned_host_id: pinned,
        };
        let (picked, report) = host_scheduling::select_host(&requirements, &slots);
        match picked {
            Some(host_id) => hosts
                .into_iter()
                .find(|h| h.id == host_id)
                .ok_or_else(|| anyhow!("selected host '{host_id}' is missing from the registry")),
            None => Err(anyhow!(
                "no eligible host for execution {}: {}",
                execution.id,
                summarize_ineligibility(&report),
            )),
        }
    }

    pub(super) async fn schedule_execution(self: &Arc<Self>, execution: &WorkExecution, worker_id: &str) -> Result<()> {
        // Double-spawn guard (Bug A): if another execution for this
        // work_item is already live (running or waiting_human), this
        // execution is a redundant duplicate created by the orphan sweep
        // racing with a still-active pane. Abandon it without spawning
        // so "execution run completed" doesn't fire prematurely.
        match self
            .work_db
            .get_live_execution_for_work_item(&execution.work_item_id, &execution.id)
        {
            Ok(Some(live)) => {
                // Liveness gate (waiting_human-zombie fix, 2026-06-14 incident):
                // `get_live_execution_for_work_item` returns any row in
                // `status IN ('running','waiting_human')`, but that is a *paper*
                // liveness signal. A row can sit `waiting_human` forever after
                // its worker died without a `Stop` hook — e.g. the cube
                // workspace-root migration relocated the pool out from under
                // three running triage panes, so their rows stayed `waiting_human`
                // and every subsequent fire died right here with `redundant_spawn`.
                // Before treating this execution as a redundant duplicate, verify
                // the blocker is *actually* live: a local execution whose worker
                // pane is provably gone (workspace dir vanished, recorded pane pid
                // dead, or a pane that never attached) is a zombie — reconcile it
                // to a terminal status and proceed with this spawn instead of
                // blocking. This is the restart-robust check that keeps the guard
                // from deferring forever to a corpse (the recurring 2026-07-03
                // redundant_spawn spam).
                let reconciled_lost_workspace = crate::lost_workspace_sweep::reconcile_if_execution_dead(
                    self.work_db.as_ref(),
                    self.dispatch_events.as_ref(),
                    &live,
                )
                .await;
                let reconciled_dead_pane = !reconciled_lost_workspace
                    && crate::dead_pane_sweep::reconcile_if_pane_dead(
                        self.work_db.as_ref(),
                        self.dispatch_events.as_ref(),
                        &live,
                        boss_engine_utils::epoch_time::now_epoch_secs(),
                    )
                    .await;
                if reconciled_lost_workspace || reconciled_dead_pane {
                    tracing::warn!(
                        execution_id = %execution.id,
                        reconciled_execution_id = %live.id,
                        work_item_id = %execution.work_item_id,
                        reason = if reconciled_dead_pane { "pane_dead" } else { "workspace_lost" },
                        "spawn_attempt: prior 'live' execution's worker pane was gone; \
                         reconciled it and proceeding with this spawn",
                    );
                    // Not redundant after all — fall through to the rest of dispatch.
                } else {
                    // The blocker survived every death check the reconciler
                    // applies, so it is genuinely live: this fire is redundant
                    // *normal* scheduler behaviour (the work is already running),
                    // NOT a failure. Annotate the blocker's liveness verdict +
                    // age-in-status so the next recurrence is attributable in one
                    // read.
                    let live_age_secs = live
                        .started_at
                        .as_deref()
                        .and_then(|s| s.parse::<i64>().ok())
                        .map(|s| boss_engine_utils::epoch_time::now_epoch_secs().saturating_sub(s));
                    tracing::info!(
                        execution_id = %execution.id,
                        live_execution_id = %live.id,
                        work_item_id = %execution.work_item_id,
                        live_execution_age_secs = ?live_age_secs,
                        "spawn_attempt: work already running in a live execution; skipping this fire (not an error)",
                    );
                    if let Err(err) = self.work_db.mark_execution_redundant(&execution.id) {
                        tracing::error!(
                            execution_id = %execution.id,
                            ?err,
                            "spawn_attempt: failed to mark redundant execution abandoned",
                        );
                    }
                    // Neutral automation bookkeeping: an `automation_triage` fire
                    // superseded by a genuinely-live execution records
                    // `triage_running` — the automation IS being triaged, just by
                    // the live execution, not this one. This overwrites the
                    // pessimistic "dispatched; awaiting triage worker decision"
                    // placeholder with a *neutral* outcome so the automation UI
                    // renders "Running" (blue), NOT "Failed (retrying)" (which is
                    // reserved for dispatch failures that will not self-heal — a
                    // redundant fire self-heals when the live execution finishes).
                    if execution.kind == ExecutionKind::AutomationTriage
                        && let Err(err) = self.work_db.finalize_automation_triage_run(
                            &execution.id,
                            boss_protocol::AUTOMATION_OUTCOME_TRIAGE_RUNNING,
                            None,
                            Some(&format!(
                                "skipped: work already running in live execution {} (this fire was redundant, \
                                 not a failure)",
                                live.id
                            )),
                        )
                    {
                        tracing::warn!(
                            execution_id = %execution.id,
                            ?err,
                            "spawn_attempt: failed to record already-running outcome on automation_runs row",
                        );
                    }
                    // Emit a terminal event so the dispatch timeline doesn't
                    // silently stall at `worker_claimed/ok` for 30s until the
                    // watchdog fires. The execution is already marked redundant
                    // (terminal DB state); `host_selected:error` remains the
                    // timeline closer (`is_terminal_event` keys off `error`), but
                    // its details carry `live_execution_liveness: "alive"` +
                    // `live_execution_age_secs` so the diagnostic stream shows the
                    // block was against a genuinely-live execution, not a corpse.
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_worker(worker_id)
                                .with_details(serde_json::json!({
                                    "reason": "redundant_spawn",
                                    "live_execution_id": live.id,
                                    "live_execution_liveness": "alive",
                                    "live_execution_age_secs": live_age_secs,
                                })),
                        )
                        .await;
                    return Err(anyhow::anyhow!(
                        "redundant spawn: execution {} for work_item {} superseded by live execution {}",
                        execution.id,
                        execution.work_item_id,
                        live.id,
                    ));
                }
            }
            Ok(None) => {}
            Err(err) => {
                // Non-fatal: if the DB check fails, proceed with the
                // spawn rather than blocking all dispatches. The worst
                // case is the double-spawn race we're trying to prevent,
                // which is the pre-existing behaviour.
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "spawn_attempt: live-execution check failed — proceeding without dedup guard",
                );
            }
        }

        // Per-PR single-writer guard (T1577 / T1815 incident). The
        // double-spawn guard above only sees executions on this exact
        // work_item; it cannot see a *sibling* execution that targets the
        // SAME PR. A revision (conflict-resolution, ci-fix, review-findings,
        // operator) is a distinct work item whose chain root owns the PR,
        // and cube co-locates every same-PR worker on ONE shared jj backing
        // store — so a second live execution anywhere in the chain rebases
        // and rewrites the first's commits. Every dispatch entry point
        // funnels through here, so this one check serializes ALL of them:
        // the conflict-resolution and ci-fix auto-spawn paths included.
        //
        // The auto-dispatcher (`drain_ready_queue`) applies this same guard
        // BEFORE claiming a worker (so it never wastes a slot or emits a
        // misleading `worker_claimed` timeline for a deferred row); this
        // copy is the backstop for `force_dispatch` (`bossctl agents
        // launch`) and any future direct caller, closing the chokepoint.
        //
        // Unlike the redundant-duplicate guard above, a chain sibling is NOT
        // redundant — it has its own real work — so we DEFER rather than
        // abandon: the execution stays `ready` and is re-attempted on the
        // next scheduler kick (which fires when the live sibling reaps), so
        // it runs strictly after the live one finishes.
        //
        // Goes through `resolve_chain_hold` (not the raw `WorkDb` query) so
        // this backstop shares the pre-claim guard's zombie reconciliation —
        // see that method's docs — and its review-yields-to-conflict-fix
        // carve-out, so a merge-conflict revision the pre-claim check in
        // `drain_ready_queue` just bypassed isn't immediately re-deferred
        // here (which would otherwise wedge it in a defer loop instead of
        // ever actually dispatching).
        match self.resolve_chain_hold(execution).await {
            Ok(ChainHold::Blocked {
                sibling, review_held, ..
            }) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    live_sibling_execution_id = %sibling.id,
                    live_sibling_work_item_id = %sibling.work_item_id,
                    review_held,
                    "spawn_attempt: deferred — another execution on the same PR/chain is live; \
                     serializing behind it rather than co-dispatching onto the shared jj store",
                );
                // Leave the execution `ready` (do NOT abandon). The caller
                // releases the claimed worker on this `Err`, and the next
                // kick re-evaluates the still-`ready` row.
                //
                // Emit a terminal event so the dispatch timeline advances
                // past `worker_claimed/ok` immediately — otherwise the
                // stall watchdog fires ~30s later, masking the real reason
                // (chain serialization) in the timeline. The execution is
                // not actually failed; on the next kick it will re-attempt
                // and may succeed. The `error` outcome is necessary here
                // because `is_terminal_event` only recognises `outcome ==
                // "error"` (besides `pane_spawned/ok`) as closing the stage.
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_details(serde_json::json!({
                                "reason": "chain_serialized_backstop",
                                "review_held": review_held,
                                "live_sibling_execution_id": sibling.id,
                                "live_sibling_work_item_id": sibling.work_item_id,
                            })),
                    )
                    .await;
                return Err(anyhow::anyhow!(
                    "serialized: execution {} for work_item {} deferred behind live chain sibling {} (work_item {})",
                    execution.id,
                    execution.work_item_id,
                    sibling.id,
                    sibling.work_item_id,
                ));
            }
            Ok(ChainHold::ReviewBypassed(sibling)) => {
                tracing::info!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    live_sibling_execution_id = %sibling.id,
                    live_sibling_work_item_id = %sibling.work_item_id,
                    "spawn_attempt: proceeding — live sibling is a read-only pr_review, \
                     bypassed for this merge-conflict revision",
                );
            }
            Ok(ChainHold::Clear) => {}
            Err(err) => {
                // Non-fatal: proceed rather than blocking all dispatches.
                // The post-lease assertion below is the defense-in-depth
                // backstop for the single-writer invariant.
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "spawn_attempt: chain single-writer check failed — proceeding without serialization guard",
                );
            }
        }

        // At-dispatch gate check: if the work item is still gated by an
        // unmet prerequisite, the execution must not be dispatched. This
        // closes the timing window where a `ready` row was created before
        // a `blocks` dep edge committed (or before the gate check in
        // `reconcile_work_item_execution` ran). Downgrade to
        // `waiting_dependency` so the execution leaves the ready queue
        // and gets re-promoted when the gate clears.
        match self.work_db.gating_prereqs_for(&execution.work_item_id) {
            Ok(prereqs) if !prereqs.is_empty() => {
                let names = prereqs.join(", ");
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    gating_prereqs = %names,
                    "spawn_attempt: execution for gated work item — downgrading to waiting_dependency and skipping dispatch",
                );
                if let Err(err) = self.work_db.downgrade_ready_to_waiting_dependency(&execution.id) {
                    tracing::error!(
                        execution_id = %execution.id,
                        ?err,
                        "spawn_attempt: failed to downgrade gated execution",
                    );
                }
                // Emit a terminal event so the dispatch timeline advances
                // past `worker_claimed/ok` immediately and the stall
                // watchdog doesn't misattribute the hold to worker claim.
                // The execution is downgraded to `waiting_dependency` (not
                // failed); on gate clearance `dep_unblock_sweep` re-promotes
                // it to `ready` and the next kick re-dispatches.
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_details(serde_json::json!({
                                "reason": "gating_prereqs_blocked",
                                "gating_prereqs": prereqs,
                            })),
                    )
                    .await;
                return Err(anyhow::anyhow!(
                    "gated: execution {} for {} blocked by [{}]",
                    execution.id,
                    execution.work_item_id,
                    names,
                ));
            }
            Ok(_) => {}
            Err(err) => {
                // Non-fatal: proceed rather than blocking all dispatches.
                // The work item's gating state is re-evaluated on the next
                // kick, so a transient DB error here at most allows one
                // erroneous dispatch.
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "spawn_attempt: gate check failed — proceeding without gating guard",
                );
            }
        }

        let work_item = match self
            .resolve_execution_work_item(execution)
            .with_context(|| format!("failed to resolve work item {}", execution.work_item_id))
        {
            Ok(work_item) => work_item,
            Err(err) => {
                // Previously a bare `?`: the execution returned to the
                // drain loop with no dispatch event and no start-failure
                // record, so it sat at `worker_claimed` until the stall
                // watchdog reaped it ~30s later. Emit a terminal
                // `host_selected:error` so the timeline names the blocker
                // and the watchdog stops re-flagging it, then record the
                // start failure so the row flips out of `worker_claimed`
                // immediately.
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_error(&err)
                            .with_details(serde_json::json!({ "reason": "work_item_unresolved" })),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    None,
                    ("work_item_unresolved", "Could not resolve work item for execution"),
                    &err,
                )?;
                return Err(err);
            }
        };
        let task = execution_task_summary(execution, &work_item);

        // Merge-conflict revision already-resolved guard: a revision
        // spawned to fix a merge conflict can sit `ready` for a while
        // (worker-pool contention) before a slot frees up. In that window
        // the periodic merge-poller sweep may independently notice the
        // bound PR is mergeable again and retire the linked
        // `conflict_resolutions` ledger row to `succeeded`
        // (`conflict_watch::on_resolved`) without ever touching this
        // now-unnecessary revision task/execution. Dispatching a worker
        // here would just have it discover "nothing to do" and become the
        // produce-a-PR nudge loop described in the `nudge_breaker` module
        // doc. Check the ledger (already kept fresh by that sweep) before
        // ever leasing a workspace.
        if execution.kind == ExecutionKind::RevisionImplementation
            && let WorkItem::Task(ref revision_task) = work_item
            && revision_task.kind == TaskKind::Revision
            && let Some(crz_id) = revision_task
                .created_via
                .strip_prefix(boss_protocol::CREATED_VIA_MERGE_CONFLICT_PREFIX)
        {
            match self.work_db.get_conflict_resolution(crz_id) {
                Ok(Some(ref attempt)) if attempt.status == "succeeded" => {
                    tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        attempt_id = %attempt.id,
                        "spawn_attempt: merge-conflict revision's bound PR was already resolved \
                         before dispatch — retiring without spawning a worker",
                    );
                    match self
                        .work_db
                        .retire_stale_revision_before_dispatch(&execution.id, &revision_task.id)
                    {
                        Ok(task_transitioned) => {
                            if task_transitioned {
                                self.publisher
                                    .publish_work_item_changed(
                                        &revision_task.product_id,
                                        &revision_task.id,
                                        "merge_conflict_already_resolved",
                                    )
                                    .await;
                            }
                            self.dispatch_events
                                .emit(
                                    DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                                        .with_work_item(&execution.work_item_id)
                                        .with_worker(worker_id)
                                        .with_details(serde_json::json!({
                                            "reason": "merge_conflict_already_resolved",
                                            "attempt_id": attempt.id,
                                        })),
                                )
                                .await;
                            return Err(anyhow::anyhow!(
                                "skipped: execution {} for work_item {} not spawned — merge conflict \
                                 already resolved before dispatch (attempt {})",
                                execution.id,
                                execution.work_item_id,
                                attempt.id,
                            ));
                        }
                        Err(err) => {
                            tracing::warn!(
                                execution_id = %execution.id,
                                ?err,
                                "spawn_attempt: failed to retire stale already-resolved revision; \
                                 proceeding with dispatch",
                            );
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        execution_id = %execution.id,
                        crz_id,
                        ?err,
                        "spawn_attempt: failed to look up linked conflict_resolution; proceeding with dispatch",
                    );
                }
            }
        }

        // Host selection (distributed-execution PR3): pick the host this
        // execution should run on, then build the matching adapter (local
        // vs SSH-remote) and route the whole dispatch through it. A
        // no-eligible-host result is a recoverable pre-start failure — it
        // backs off and raises an attention item rather than hot-looping,
        // and a later kick retries once a host comes online / tags change.
        let selected_host = match self.select_host_for_execution(execution, &work_item) {
            Ok(host) => host,
            Err(err) => {
                // No event was emitted here before, so a `no_eligible_host`
                // failure was invisible in the per-execution timeline — the
                // watchdog reaped it as a `worker_claimed` stall. Emit a
                // terminal `host_selected:error` so the blocker is named in
                // dispatch.jsonl before recording the start failure.
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_error(&err)
                            .with_details(serde_json::json!({ "reason": "no_eligible_host" })),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    None,
                    ("no_eligible_host", "No eligible host for execution"),
                    &err,
                )?;
                return Err(err);
            }
        };
        let adapter = match self.host_adapter_provider.adapter_for(&selected_host).await {
            Ok(adapter) => adapter,
            Err(err) => {
                // Same silent-gap fix as the host-selection branch above:
                // a host was chosen but its adapter could not be built
                // (e.g. SSH unreachable). Make it observable + terminal.
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_error(&err)
                            .with_details(serde_json::json!({
                                "reason": "host_adapter_unavailable",
                                "host_id": selected_host.id.clone(),
                            })),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    None,
                    ("host_adapter_unavailable", "Could not build host adapter"),
                    &err,
                )?;
                return Err(err);
            }
        };
        // Host chosen and adapter ready: emit the success milestone so the
        // claimed -> repo-ensure handoff is no longer a blind spot in the
        // timeline (closes the gap that hid the automation-pool stall).
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_details(serde_json::json!({ "host_id": selected_host.id.clone() })),
            )
            .await;
        tracing::info!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            host_id = %selected_host.id,
            "host-selection: routing execution to host",
        );

        // Mirror the argv `ensure_repo` actually drives so the dispatch-event
        // `cube_command` is reproducible from a terminal: a bare resolver
        // slug goes positionally (`repo ensure <name>`), a URL via `--origin`.
        let ensure_args = crate::repo_slug::repo_ensure_args(&execution.repo_remote_url);
        // Record the attempt *before* the subprocess, mirroring
        // `cube_workspace_lease_attempted`. `cube repo ensure` on a cold
        // repo can outrun the `worker_claimed` stall threshold; with this
        // marker the watchdog attributes such a stall to the ensure
        // subprocess instead of the (already-completed) worker claim.
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::CubeRepoEnsureAttempted, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_cube_invocation(adapter.command_repr(&ensure_args))
                    .with_details(serde_json::json!({
                        "repo_remote_url": execution.repo_remote_url,
                        "timeout_ms": CUBE_REPO_ENSURE_TIMEOUT.as_millis() as u64,
                    })),
            )
            .await;
        let repo = match tokio::time::timeout(
            CUBE_REPO_ENSURE_TIMEOUT,
            adapter.ensure_repo(&execution.repo_remote_url),
        )
        .await
        {
            Ok(Ok(repo)) => repo,
            Ok(Err(err)) => {
                self.emit_ensure_failed_and_record(
                    execution,
                    worker_id,
                    adapter.command_repr(&ensure_args),
                    &err,
                    serde_json::json!({ "host_id": selected_host.id.clone() }),
                    ("cube_repo_ensure_failed", "Cube `repo ensure` failed"),
                )
                .await?;
                return Err(err);
            }
            Err(_elapsed) => {
                let err = anyhow!(
                    "cube `repo ensure` timed out after {}s",
                    CUBE_REPO_ENSURE_TIMEOUT.as_secs()
                );
                self.emit_ensure_failed_and_record(
                    execution,
                    worker_id,
                    adapter.command_repr(&ensure_args),
                    &err,
                    serde_json::json!({
                        "reason": "timeout",
                        "timeout_ms": CUBE_REPO_ENSURE_TIMEOUT.as_millis() as u64,
                        "host_id": selected_host.id.clone(),
                    }),
                    ("cube_repo_ensure_failed", "Cube `repo ensure` timed out"),
                )
                .await?;
                return Err(err);
            }
        };
        self.maybe_probe_cold_repo(execution, &adapter).await;
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::CubeRepoEnsured, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_cube_repo(&repo.repo_id)
                    .with_cube_invocation(adapter.command_repr(&ensure_args)),
            )
            .await;

        // PR number to pass to `cube workspace goto` after the lease.
        // Set for pr_review and revision_implementation executions that have a PR URL.
        let pr_for_goto: Option<u64> = match execution.kind {
            ExecutionKind::RevisionImplementation => execution
                .pr_url
                .as_deref()
                .and_then(boss_github::pr_url::pr_number_from_url)
                // `execution.pr_url` is not reliably stamped on every revision dispatch
                // path (e.g. orphan-sweep re-dispatch, user-initiated `bossctl work start`).
                // Fall back to the chain root's PR URL — the same authoritative lookup
                // used by completion.rs — so positioning is never skipped for revisions.
                .or_else(|| {
                    self.work_db
                        .get_revision_chain_root_pr_url(&execution.work_item_id)
                        .as_deref()
                        .and_then(boss_github::pr_url::pr_number_from_url)
                }),
            ExecutionKind::PrReview => match &work_item {
                WorkItem::Task(task) | WorkItem::Chore(task) => task
                    .pr_url
                    .as_deref()
                    .filter(|u| !u.is_empty())
                    .and_then(boss_github::pr_url::pr_number_from_url),
                _ => None,
            },
            _ => None,
        };

        let lease = match self
            .lease_workspace_with_fallback(execution, worker_id, &repo, &task, &adapter)
            .await
        {
            Ok(lease) => lease,
            Err(err) => {
                // The lease helper has already emitted attempt /
                // failure events for every try; convert the final
                // failure into the start-failure record so the
                // execution row flips to `failed` cleanly instead of
                // wedging in `worker_claimed`.
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    Some(repo.repo_id.as_str()),
                    ("cube_workspace_lease_failed", "Cube `workspace lease` failed"),
                    &err,
                )?;
                return Err(err);
            }
        };

        // Recovery, cube-first (see `reconcile_workspace_recovery`). Runs
        // before the run starts so the worker's prompt can read the marker
        // this drops. A no-op for every non-resume dispatch.
        self.reconcile_workspace_recovery(execution, worker_id, &lease).await;

        // Lease-time occupancy guard (defect 3). Cube should never hand us
        // a workspace that is still the cwd of a live worker — but the
        // duplicate-dispatch incident proved it can when an upstream bug
        // frees a lease while the worker's process is still alive. Before
        // we commit this run to the workspace, refuse (and loudly log) if
        // the engine's own live-worker registry still tracks a live
        // process there. A refused lease retries via the normal pre-start
        // backoff; an interleaved working copy silently corrupts two
        // workers' edits. Only runs when the registry is wired
        // (production); fails open otherwise. The probe is keyed by
        // run_id/execution_id, so it never trips on our own (not-yet-
        // spawned) execution.
        if let Some(live_states) = self.live_worker_states.as_ref() {
            let snapshot = live_states.snapshot();
            let occupant = occupying_live_worker(
                &lease.workspace_id,
                &execution.id,
                &snapshot,
                |eid| self.work_db.get_execution(eid).ok().and_then(|e| e.cube_workspace_id),
                |pid| {
                    !matches!(
                        crate::dead_pid_sweep::probe_pid(pid),
                        crate::dead_pid_sweep::PidStatus::Dead
                    )
                },
            );
            if let Some(occupant_run_id) = occupant {
                tracing::error!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id,
                    cube_workspace_id = %lease.workspace_id,
                    workspace_path = %lease.workspace_path.display(),
                    occupied_by = %occupant_run_id,
                    "REFUSING lease: cube returned a workspace still occupied by a live tracked worker \
                     — refusing rather than interleaving two workers in one working copy (defect 3)",
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeWorkspaceLeaseFailed, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_details(serde_json::json!({
                                "reason": "workspace_occupied_by_live_worker",
                                "occupied_by_execution_id": occupant_run_id,
                                "workspace_path": lease.workspace_path.display().to_string(),
                            })),
                    )
                    .await;
                // Record this workspace as refused so the next lease call
                // passes --exclude and skips it, breaking the livelock where
                // cube's deterministic ordering re-offers the same occupied
                // workspace on every retry attempt.
                self.refused_workspaces
                    .lock()
                    .await
                    .entry(execution.id.clone())
                    .or_default()
                    .push(lease.workspace_id.clone());

                // Hand the workspace straight back so it isn't stranded.
                if let Err(release_err) = adapter.release_workspace(&lease.lease_id).await {
                    tracing::error!(
                        ?release_err,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after refusing an occupied lease",
                    );
                }
                let err = anyhow!(
                    "leased workspace {} is occupied by live worker {}",
                    lease.workspace_id,
                    occupant_run_id
                );
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    Some(repo.repo_id.as_str()),
                    (
                        "cube_workspace_occupied",
                        "Cube leased a workspace occupied by a live worker",
                    ),
                    &err,
                )?;
                return Err(err);
            }
        }

        // Per-PR single-writer assertion (defense in depth). The pre-claim
        // chain guard at the top of `schedule_execution` already deferred
        // any execution whose PR/chain has a live sibling, but there is a
        // TOCTOU window between that check and committing this run to the
        // leased workspace: a sibling could have gone live in between. We
        // re-assert the invariant HERE, immediately before spawning onto the
        // shared jj backing store — the irreversible step. The occupancy
        // guard above only catches a sibling in the SAME workspace; two
        // same-PR workers in DIFFERENT cube workspaces still share one
        // backing store and corrupt each other, which this catches. On a
        // violation we release the lease and refuse rather than interleave.
        //
        // Goes through `resolve_chain_hold` for the same reason as the other
        // two call sites: a `waiting_human` "sibling" that is actually a
        // dead worker must not refuse this spawn forever, and a live
        // `pr_review` sibling must not refuse a merge-conflict revision the
        // earlier checkpoints already bypassed for it (see
        // `resolve_chain_hold`'s docs) — otherwise this defense-in-depth
        // assertion would defeat the bypass right before the irreversible
        // spawn step.
        match self.resolve_chain_hold(execution).await {
            Ok(ChainHold::Blocked {
                sibling, review_held, ..
            }) => {
                tracing::error!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id,
                    cube_workspace_id = %lease.workspace_id,
                    live_sibling_execution_id = %sibling.id,
                    live_sibling_work_item_id = %sibling.work_item_id,
                    review_held,
                    "REFUSING spawn: another execution on the same PR/chain went live after the \
                     pre-claim guard — refusing rather than handing two same-PR workers the shared \
                     jj backing store (single-writer invariant)",
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeWorkspaceLeaseFailed, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_details(serde_json::json!({
                                "reason": "chain_sibling_went_live",
                                "review_held": review_held,
                                "live_sibling_execution_id": sibling.id,
                                "live_sibling_work_item_id": sibling.work_item_id,
                            })),
                    )
                    .await;
                // Hand the workspace back so it isn't stranded, then leave
                // the execution `ready` (the deferral path) so it re-attempts
                // once the sibling reaps.
                if let Err(release_err) = adapter.release_workspace(&lease.lease_id).await {
                    tracing::error!(
                        ?release_err,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after refusing a chain-sibling-racing spawn",
                    );
                }
                return Err(anyhow!(
                    "serialized: execution {} for work_item {} refused after lease — chain sibling {} (work_item {}) went live",
                    execution.id,
                    execution.work_item_id,
                    sibling.id,
                    sibling.work_item_id,
                ));
            }
            Ok(ChainHold::ReviewBypassed(_)) | Ok(ChainHold::Clear) => {}
            Err(err) => {
                // Fail open: a DB error here must not wedge dispatch. The
                // pre-claim guard already covered the common case.
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "spawn_attempt: post-lease chain single-writer assertion failed to query — proceeding",
                );
            }
        }
        {
            let mut lease_args = vec![
                "--json",
                "workspace",
                "lease",
                repo.repo_id.as_str(),
                "--task",
                task.as_str(),
                // Mirror the flag the actual lease call passes (see
                // `cube_commands::lease_workspace`) so this diagnostic repr
                // reproduces exactly what ran.
                "--release-on-setup-failure",
            ];
            if let Some(p) = execution.preferred_workspace_id.as_deref() {
                lease_args.extend_from_slice(&["--prefer", p]);
            }
            self.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::CubeWorkspaceLeased, DispatchOutcome::Ok, &execution.id)
                        .with_work_item(&execution.work_item_id)
                        .with_worker(worker_id)
                        .with_cube_repo(&repo.repo_id)
                        .with_cube_lease(&lease.lease_id)
                        .with_cube_workspace(&lease.workspace_id)
                        .with_cube_invocation(adapter.command_repr(&lease_args)),
                )
                .await;
        }
        let change_title = execution_change_title(execution, &work_item);

        // For PR-targeting executions, run `cube workspace goto --workspace <path>
        // --pr <n>` AFTER the lease to position the working copy on the PR branch
        // head. This must happen before handing the workspace to the worker.
        // If positioning fails, abort dispatch with a diagnosable stage.
        if let Some(pr) = pr_for_goto {
            let goto_repr = adapter.command_repr(&[
                "--json",
                "workspace",
                "goto",
                "--workspace",
                &lease.workspace_path.display().to_string(),
                "--pr",
                &pr.to_string(),
            ]);
            match adapter.goto_workspace(&lease.workspace_path, pr).await {
                Ok(()) => {
                    tracing::info!(
                        execution_id = %execution.id,
                        kind = execution.kind.as_str(),
                        workspace_path = %lease.workspace_path.display(),
                        pr,
                        "workspace positioned via cube workspace goto",
                    );
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::CubeWorkspacePositioned, DispatchOutcome::Ok, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_worker(worker_id)
                                .with_cube_repo(&repo.repo_id)
                                .with_cube_lease(&lease.lease_id)
                                .with_cube_workspace(&lease.workspace_id)
                                .with_cube_invocation(goto_repr)
                                .with_details(serde_json::json!({
                                    "pr": pr,
                                    "kind": execution.kind.as_str(),
                                })),
                        )
                        .await;
                }
                Err(err) => {
                    if let Err(release_err) = adapter.release_workspace(&lease.lease_id).await {
                        tracing::error!(
                            ?release_err,
                            lease_id = %lease.lease_id,
                            "failed to release workspace after goto positioning failure"
                        );
                    }
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(
                                Stage::CubeWorkspacePositioningFailed,
                                DispatchOutcome::Error,
                                &execution.id,
                            )
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_error(&err)
                            .with_cube_invocation(goto_repr),
                        )
                        .await;
                    self.record_start_failure(
                        Arc::clone(self),
                        execution,
                        worker_id,
                        Some(repo.repo_id.as_str()),
                        (
                            "cube_workspace_positioning_failed",
                            "Cube `workspace goto` positioning failed",
                        ),
                        &err,
                    )?;
                    return Err(err);
                }
            }
        }

        // For PR-targeting executions the workspace is now positioned on the PR
        // head — skip create_change (there is nothing to create; the worker edits
        // or reviews the branch directly). For all other executions create a fresh
        // jj change via `cube change create`.
        let change: Option<CubeChangeHandle> = if pr_for_goto.is_some() {
            None
        } else {
            // Normal path (pr_review without a PR URL, and all non-review/
            // non-revision executions): create a fresh jj change via `cube
            // change create`.
            let workspace_path_str = lease.workspace_path.display().to_string();
            let change_repr: Option<(String, String)> = adapter.command_repr(&[
                "--json",
                "change",
                "create",
                "--workspace",
                &workspace_path_str,
                "--title",
                &change_title,
            ]);
            match adapter.create_change(&lease.workspace_path, &change_title).await {
                Ok(change) => {
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::CubeChangeCreated, DispatchOutcome::Ok, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_worker(worker_id)
                                .with_cube_repo(&repo.repo_id)
                                .with_cube_lease(&lease.lease_id)
                                .with_cube_workspace(&lease.workspace_id)
                                .with_cube_invocation(change_repr)
                                .with_details(serde_json::json!({
                                    "change_id": change.change_id,
                                    "change_title": change_title,
                                })),
                        )
                        .await;
                    Some(change)
                }
                Err(err) => {
                    if let Err(release_err) = adapter.release_workspace(&lease.lease_id).await {
                        tracing::error!(
                            ?release_err,
                            lease_id = %lease.lease_id,
                            "failed to release workspace after change creation failure"
                        );
                    }
                    self.dispatch_events
                        .emit(
                            DispatchEvent::new(Stage::CubeChangeCreated, DispatchOutcome::Error, &execution.id)
                                .with_work_item(&execution.work_item_id)
                                .with_worker(worker_id)
                                .with_cube_repo(&repo.repo_id)
                                .with_cube_lease(&lease.lease_id)
                                .with_cube_workspace(&lease.workspace_id)
                                .with_error(&err)
                                .with_cube_invocation(change_repr.clone()),
                        )
                        .await;
                    self.record_start_failure(
                        Arc::clone(self),
                        execution,
                        worker_id,
                        Some(repo.repo_id.as_str()),
                        ("cube_change_create_failed", "Cube `change create` failed"),
                        &err,
                    )?;
                    return Err(err);
                }
            }
        };

        match self.work_db.start_execution_run_on_host(
            &execution.id,
            worker_id,
            &repo.repo_id,
            &lease.lease_id,
            &lease.workspace_id,
            &lease.workspace_path.display().to_string(),
            &selected_host.id,
        ) {
            Ok((execution, run)) => {
                let worker_id_owned = worker_id.to_owned();
                tracing::info!(
                    execution_id = %execution.id,
                    run_id = %run.id,
                    worker_id,
                    cube_repo_id = %repo.repo_id,
                    cube_lease_id = %lease.lease_id,
                    cube_workspace_id = %lease.workspace_id,
                    cube_change_id = ?change.as_ref().map(|c| &c.change_id),
                    workspace_path = %lease.workspace_path.display(),
                    "started execution run"
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::RunStarted, DispatchOutcome::Ok, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_details(serde_json::json!({
                                "run_id": run.id,
                            })),
                    )
                    .await;
                self.publisher
                    .publish(
                        &execution.id,
                        &execution.work_item_id,
                        execution.status.as_str(),
                        "execution_started",
                    )
                    .await;
                // Auto-advance bumped `tasks.status` to `'active'`
                // inside the same transaction. Broadcast a work-tree
                // invalidation so kanban subscribers re-fetch and
                // move the card to the Doing column.
                if let Ok(work_item) = self.resolve_execution_work_item(&execution) {
                    self.publisher
                        .publish_work_item_changed(
                            work_item.product_id(),
                            &execution.work_item_id,
                            "execution_started_auto_advance",
                        )
                        .await;
                }
                // For automation triage executions, advance the
                // automation_runs row from its queued/pessimistic state
                // (`pool_throttled` or `failed_will_retry`) to
                // `triage_running` now that a pool slot is held and the
                // agent is about to start. The completion handler will
                // overwrite this with the terminal outcome.
                if execution.kind == ExecutionKind::AutomationTriage
                    && let Err(err) = self.work_db.mark_automation_run_triage_started(&execution.id)
                {
                    tracing::warn!(
                        execution_id = %execution.id,
                        ?err,
                        "failed to mark automation run triage_running on start",
                    );
                }
                // Resume-bounce SHA-delta gate: capture the bound
                // chore PR's head SHA into the execution row BEFORE
                // the worker spawns and starts pushing. The Stop
                // boundary uses this snapshot to decide whether the
                // run contributed to the bound PR. Best-effort: the
                // hook logs and swallows every failure mode (no
                // bound PR, slug/number parse failure, GitHub fetch
                // failure), and the gate treats a missing snapshot
                // as "inapplicable" — never noisier than the
                // pre-change behaviour.
                self.execution_started_hook.on_execution_started(&execution.id).await;
                let coordinator = self.clone();
                tokio::spawn(async move {
                    coordinator
                        .run_execution(execution, run, work_item, worker_id_owned, lease, change, adapter)
                        .await;
                });
                Ok(())
            }
            Err(err) => {
                let release_result = adapter.release_workspace(&lease.lease_id).await;
                if let Err(release_err) = release_result {
                    tracing::error!(
                        ?release_err,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after run start failure"
                    );
                }
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::RunStarted, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_error(&err),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    Some(repo.repo_id.as_str()),
                    ("execution_run_start_failed", "`start_execution_run` failed"),
                    &err,
                )?;
                Err(err)
            }
        }
    }

    /// Cold-repo probe (design doc Q6, Follow-up chore #8). The first
    /// time a given repo URL flows through `ensure_repo` in this
    /// engine's lifetime, ask cube `repo list --json` once and check
    /// whether the entry for this URL is sitting on cube's
    /// auto-provisioned defaults — i.e. nothing was customised with
    /// `cube repo add` / `cube repo configure`. If so, raise an
    /// advisory `repo_cold_pool` `WorkAttentionItem` against the
    /// execution naming the exact override command.
    ///
    /// Best-effort by design: never blocks dispatch, never returns an
    /// error to the caller. A failed `list_repos` round-trip is logged
    /// at WARN and the URL is still marked seen so we don't retry the
    /// probe every dispatch — engine restart re-probes per R4.
    async fn maybe_probe_cold_repo(self: &Arc<Self>, execution: &WorkExecution, adapter: &Arc<dyn HostAdapter>) {
        let origin = execution.repo_remote_url.clone();
        {
            let mut seen = self.repo_cold_probe_seen.lock().await;
            if !seen.insert(origin.clone()) {
                return;
            }
        }

        let repos = match adapter.list_repos().await {
            Ok(repos) => repos,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    execution_id = %execution.id,
                    repo_remote_url = %origin,
                    "cold-repo probe: `cube repo list` failed — skipping advisory check"
                );
                return;
            }
        };

        let Some(repo) = repos.iter().find(|r| r.origin == origin) else {
            tracing::debug!(
                execution_id = %execution.id,
                repo_remote_url = %origin,
                "cold-repo probe: ensured repo not present in `cube repo list` snapshot"
            );
            return;
        };

        if !repo_has_default_pool_config(repo) {
            return;
        }

        let title = format!(
            "Cold cube pool for `{repo_id}` — using auto-provisioned defaults",
            repo_id = repo.repo_id,
        );
        let body = cold_repo_attention_body(repo);
        let input = CreateAttentionItemInput {
            execution_id: Some(execution.id.clone()),
            work_item_id: None,
            kind: "repo_cold_pool".to_owned(),
            status: None,
            title,
            body_markdown: body,
            resolved_at: None,
        };
        match self.work_db.create_attention_item(input) {
            Ok(item) => {
                tracing::info!(
                    attention_id = %item.id,
                    execution_id = %execution.id,
                    repo_id = %repo.repo_id,
                    repo_remote_url = %origin,
                    "cold-repo probe: raised advisory attention item"
                );
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    execution_id = %execution.id,
                    repo_id = %repo.repo_id,
                    repo_remote_url = %origin,
                    "cold-repo probe: failed to persist attention item — dispatch continues"
                );
            }
        }
    }

    /// Reclaim a stale cube lease still held against `workspace_id` by a
    /// dead (now-terminal) execution, so a hard-prefer resume can
    /// re-lease that exact workspace and recover the in-flight jj
    /// checkout. See [`crate::work::WorkDb::stale_lease_to_reclaim_for_workspace`]
    /// and issue #962 for the full rationale.
    ///
    /// Best-effort: probes cube's live view (`list_workspaces`) for the
    /// lease currently bound to `workspace_id`, cross-checks it against
    /// the engine's own record (only a lease whose owning execution is
    /// terminal and unclaimed is eligible), and force-releases it. Every
    /// failure mode is logged and swallowed — the caller proceeds to the
    /// normal lease attempt regardless, so a flaky cube probe never
    /// blocks a resume.
    async fn reclaim_stale_lease_for_resume(
        &self,
        execution: &WorkExecution,
        worker_id: &str,
        workspace_id: &str,
        adapter: &Arc<dyn HostAdapter>,
    ) {
        let snapshot = match adapter.list_workspaces().await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    workspace_id,
                    error = format!("{err:#}"),
                    "stale-lease reclaim: cube workspace list failed; proceeding to lease without reclaim",
                );
                return;
            }
        };
        let Some(workspace) = snapshot.iter().find(|w| w.workspace_id == workspace_id) else {
            // Cube doesn't list the workspace, or it's already free —
            // nothing to reclaim, the lease attempt can proceed.
            return;
        };
        if workspace.state != "leased" {
            return;
        }
        let Some(current_lease_id) = workspace.lease_id.as_deref() else {
            return;
        };

        // Only reclaim a lease the engine can prove belongs to a dead
        // (terminal, unclaimed) execution for this workspace.
        let stale_lease_id = match self
            .work_db
            .stale_lease_to_reclaim_for_workspace(workspace_id, current_lease_id)
        {
            Ok(Some(id)) => id,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    workspace_id,
                    current_lease_id,
                    ?err,
                    "stale-lease reclaim: DB lookup failed; proceeding to lease without reclaim",
                );
                return;
            }
        };

        let reason = format!(
            "boss engine: reclaiming stale lease for UI-crash resume of execution {} (workspace {workspace_id})",
            execution.id,
        );
        match adapter
            .force_release_lease(&stale_lease_id, Some(reason.as_str()))
            .await
        {
            Ok(()) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    worker_id,
                    workspace_id,
                    reclaimed_lease_id = %stale_lease_id,
                    "stale-lease reclaim: force-released dead worker's lease so resume can re-lease its workspace",
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeWorkspaceLeaseAttempted, DispatchOutcome::Ok, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_details(serde_json::json!({
                                "step": "stale_lease_reclaim",
                                "workspace_id": workspace_id,
                                "reclaimed_lease_id": stale_lease_id.as_str(),
                            })),
                    )
                    .await;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    worker_id,
                    workspace_id,
                    stale_lease_id = %stale_lease_id,
                    error = format!("{err:#}"),
                    "stale-lease reclaim: force-release failed; proceeding to lease attempt anyway",
                );
            }
        }
    }

    /// Locate the recovery patch belonging to the execution that
    /// `execution` is resuming, if the engine captured one.
    ///
    /// Returns `(dead_execution_id, patch_path)`. The dead execution is found
    /// the same way [`crate::runner`] finds it for the STARTUP RECOVERY
    /// prompt block — `get_prior_orphaned_execution` — so both halves of the
    /// recovery story agree on which run is being resumed.
    ///
    /// Every failure mode (no recovery dir, no prior orphan, no patch on
    /// disk, a DB error) yields `None`: recovery is a precaution layered on
    /// top of dispatch and must never be able to break it.
    fn recovery_patch_for_resume(&self, execution: &WorkExecution) -> Option<(String, PathBuf)> {
        let recovery_dir = crate::recovery_backup::default_recovery_dir()?;
        let prior = self
            .work_db
            .get_prior_orphaned_execution(&execution.work_item_id, &execution.id)
            .ok()
            .flatten()?;
        let patch = crate::recovery_apply::find_patch(&recovery_dir, &prior.id)?;
        Some((prior.id, patch))
    }

    /// Resolve, and record, how a resume dispatch got its predecessor's work
    /// back — cube first, patch only as fallback.
    ///
    /// Called once per dispatch immediately after the lease. Does nothing for
    /// a non-resume execution (`allow_dirty` is the resume flag; the
    /// reconcilers set it on every respawn that pins a workspace).
    ///
    /// The order is the operator's:
    ///
    /// 1. **Cube recovered in place.** `lease.dirty_verified == Some(true)`
    ///    means cube handed back a working copy that still held work existing
    ///    on no remote. The work is live, in place, with jj history intact.
    ///    We must NOT apply the patch on top of it — the hunks are already
    ///    there and a second application either duplicates them or conflicts.
    /// 2. **Cube could not recover.** The lease failed and we degraded to a
    ///    free workspace, or it succeeded with `dirty_verified == Some(false)`
    ///    because the tree had already been reset. Now, and only now, replay
    ///    the patch.
    ///
    /// A failed apply is loud: an `outcome=error` dispatch event, an
    /// `ERROR`-level log, and a recovery report carrying `patch_error` so the
    /// worker's prompt tells it recovery FAILED rather than letting it build
    /// on a tree it believes was restored.
    pub(super) async fn reconcile_workspace_recovery(
        &self,
        execution: &WorkExecution,
        worker_id: &str,
        lease: &CubeWorkspaceLease,
    ) {
        if !execution.allow_dirty {
            return;
        }
        let Some((dead_execution_id, patch_path)) = self.recovery_patch_for_resume(execution) else {
            // No captured patch. Cube may still have recovered in place — say
            // so, so the worker knows not to start from `main`.
            if lease.dirty_verified == Some(true) {
                self.record_recovery(
                    execution,
                    worker_id,
                    lease,
                    crate::recovery_apply::RecoveryReport {
                        for_execution_id: execution.id.clone(),
                        from_execution_id: String::new(),
                        source: crate::recovery_apply::RecoverySource::CubeInPlace,
                        applied: None,
                        patch_error: None,
                    },
                    None,
                )
                .await;
            }
            return;
        };

        // ── 1. cube-first ────────────────────────────────────────────────
        if lease.dirty_verified == Some(true) {
            self.record_recovery(
                execution,
                worker_id,
                lease,
                crate::recovery_apply::RecoveryReport {
                    for_execution_id: execution.id.clone(),
                    from_execution_id: dead_execution_id,
                    source: crate::recovery_apply::RecoverySource::CubeInPlace,
                    applied: None,
                    patch_error: None,
                },
                Some(&patch_path),
            )
            .await;
            return;
        }

        // ── 2. patch fallback ────────────────────────────────────────────
        match crate::recovery_apply::apply_recovery_patch(&lease.workspace_path, &patch_path) {
            Ok(Some(applied)) => {
                tracing::info!(
                    execution_id = %execution.id,
                    dead_execution_id = %dead_execution_id,
                    workspace_id = %lease.workspace_id,
                    restored = %applied.summary(),
                    "workspace recovery: replayed the dead execution's patch into the resuming workspace",
                );
                self.record_recovery(
                    execution,
                    worker_id,
                    lease,
                    crate::recovery_apply::RecoveryReport {
                        for_execution_id: execution.id.clone(),
                        from_execution_id: dead_execution_id,
                        source: crate::recovery_apply::RecoverySource::Patch,
                        applied: Some(applied),
                        patch_error: None,
                    },
                    Some(&patch_path),
                )
                .await;
            }
            Ok(None) => {
                // The capture held nothing but Boss's own bookkeeping. Not a
                // failure, but not a recovery either — say nothing to the
                // worker rather than claim a restoration that did not happen.
                tracing::info!(
                    execution_id = %execution.id,
                    dead_execution_id = %dead_execution_id,
                    patch = %patch_path.display(),
                    "workspace recovery: patch held only Boss bookkeeping; nothing restored",
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::WorkspaceRecovery, DispatchOutcome::Ok, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_details(serde_json::json!({
                                "source": "patch",
                                "restored": false,
                                "reason": "bookkeeping_only",
                                "dead_execution_id": dead_execution_id,
                            })),
                    )
                    .await;
                crate::recovery_apply::mark_patch_consumed(&patch_path);
            }
            Err(err) => {
                let message = format!("{err:#}");
                tracing::error!(
                    execution_id = %execution.id,
                    dead_execution_id = %dead_execution_id,
                    workspace_id = %lease.workspace_id,
                    patch = %patch_path.display(),
                    error = %message,
                    "workspace recovery FAILED: the dead execution's patch did not apply; \
                     the worker will be told NOT to assume its state was recovered",
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::WorkspaceRecovery, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_details(serde_json::json!({
                                "source": "patch",
                                "restored": false,
                                "reason": "apply_failed",
                                "dead_execution_id": dead_execution_id,
                                "recovery_patch": patch_path.display().to_string(),
                                "error": message,
                            })),
                    )
                    .await;
                // Deliberately NOT marked consumed: the patch is the only
                // copy of the work and a human may still salvage it by hand.
                let report = crate::recovery_apply::RecoveryReport {
                    for_execution_id: execution.id.clone(),
                    from_execution_id: dead_execution_id,
                    source: crate::recovery_apply::RecoverySource::Patch,
                    applied: None,
                    patch_error: Some(message),
                };
                if let Err(write_err) = report.write(&lease.workspace_path) {
                    tracing::warn!(
                        execution_id = %execution.id,
                        error = %format!("{write_err:#}"),
                        "workspace recovery: could not write the failure marker; \
                         the worker's prompt will not mention the failed recovery",
                    );
                }
            }
        }
    }

    /// Persist a successful recovery: drop the marker the worker's prompt
    /// reads, emit the dispatch event, and retire the patch so a later
    /// restart does not replay it over the work it already restored (P4).
    async fn record_recovery(
        &self,
        execution: &WorkExecution,
        worker_id: &str,
        lease: &CubeWorkspaceLease,
        report: crate::recovery_apply::RecoveryReport,
        consume_patch: Option<&Path>,
    ) {
        let source = match report.source {
            crate::recovery_apply::RecoverySource::CubeInPlace => "cube_in_place",
            crate::recovery_apply::RecoverySource::Patch => "patch",
        };
        let restored = report.applied.as_ref().map(|a| a.summary());
        if let Err(err) = report.write(&lease.workspace_path) {
            tracing::warn!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "workspace recovery: could not write the recovery marker; the worker's \
                 prompt will not know its state was recovered",
            );
        }
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::WorkspaceRecovery, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_details(serde_json::json!({
                        "source": source,
                        "restored": true,
                        "workspace_id": lease.workspace_id,
                        "dead_execution_id": report.from_execution_id,
                        "summary": restored,
                    })),
            )
            .await;
        if let Some(patch) = consume_patch {
            crate::recovery_apply::mark_patch_consumed(patch);
        }
    }

    /// Lease a cube workspace for `execution`, emitting a structured
    /// attempt/failure event for every try and falling back to "any
    /// free workspace" when an unprefixed lease fails.
    ///
    /// Behaviour matrix:
    ///
    /// | preferred set? | first attempt      | on first failure                          |
    /// |----------------|--------------------|-------------------------------------------|
    /// | no             | without `--prefer` | retry once without `--prefer` (`any_free`) |
    /// | yes            | with `--prefer`    | terminal failure (preserves continuity)   |
    ///
    /// When `preferred_workspace_id` is set the caller needs a specific
    /// workspace (e.g. resuming a prior run). Silently landing elsewhere
    /// would lose state continuity, so we fail fast and let the scheduler
    /// retry the dispatch later. When no preference is set any free
    /// workspace is acceptable, so a single bad workspace cannot block
    /// the entire dispatch.
    ///
    /// Each subprocess invocation is bounded by [`CUBE_LEASE_TIMEOUT`]
    /// so the engine cannot wedge indefinitely waiting on cube — the
    /// motivating incident sat in `worker_claimed/ok` for ~46s with
    /// no event because the cube call never returned.
    pub(super) async fn lease_workspace_with_fallback(
        &self,
        execution: &WorkExecution,
        worker_id: &str,
        repo: &CubeRepoHandle,
        task: &str,
        adapter: &Arc<dyn HostAdapter>,
    ) -> Result<CubeWorkspaceLease> {
        let prefer = execution.preferred_workspace_id.as_deref();
        let allow_dirty = execution.allow_dirty;
        // Soft-prefer (OQ5): revision_implementation executions set
        // prefer_is_soft = true so a missing or leased preferred workspace
        // degrades silently to any free workspace rather than failing hard.
        // Orphan-resume executions use the hard "none" policy (prefer_is_soft
        // = false) because their state lives only in that specific workspace.
        // allow_dirty additionally suppresses the cube-side reset so the
        // recovering worker lands on the dirty tree; it implies a hard-fail
        // (no fallback) because the uncommitted work is only in that workspace.
        let fallback_policy = if prefer.is_none() || execution.prefer_is_soft {
            "any_free"
        } else {
            "none"
        };

        // Look up any workspaces that were refused for this execution by the
        // occupancy guard on a previous dispatch attempt. Passing them as
        // `--exclude` to cube breaks the livelock where cube's deterministic
        // candidate ordering keeps re-offering the same occupied workspace.
        let refused: Vec<String> = self
            .refused_workspaces
            .lock()
            .await
            .get(&execution.id)
            .cloned()
            .unwrap_or_default();
        let refused_refs: Vec<&str> = refused.iter().map(|s| s.as_str()).collect();

        // Stale-lease reclaim (issue #962 — UI-crash resume).
        //
        // A hard-prefer resume targets the exact workspace the dead
        // worker was leased into, because the in-flight jj checkout the
        // human wants recovered lives only there. But after a UI crash
        // the dead execution's cube lease is intentionally left intact
        // (the startup reaper preserves it), so cube still reports that
        // workspace as `leased` and will refuse a fresh
        // `--prefer <workspace>` lease — failing the resume outright and
        // stranding the local work. Before attempting the prefer lease,
        // reclaim the dead lease if (and only if) the engine can prove
        // it belongs to a now-terminal execution and no live execution
        // claims the workspace. Best-effort: any probe/reclaim error is
        // logged and we fall through to the normal lease attempt rather
        // than blocking the resume.
        if let Some(workspace_id) = prefer.filter(|_| !execution.prefer_is_soft) {
            self.reclaim_stale_lease_for_resume(execution, worker_id, workspace_id, adapter)
                .await;
        }

        // Build the lease args for attempt 1 so we can attach the
        // exact command to both the attempted and failed events.
        let mut attempt1_args = vec![
            "--json",
            "workspace",
            "lease",
            repo.repo_id.as_str(),
            "--task",
            task,
            "--release-on-setup-failure",
        ];
        if let Some(p) = prefer {
            attempt1_args.extend_from_slice(&["--prefer", p]);
        }
        if allow_dirty {
            attempt1_args.push("--allow-dirty");
        }
        for excluded in &refused_refs {
            attempt1_args.extend_from_slice(&["--exclude", excluded]);
        }
        let attempt1_repr = adapter.command_repr(&attempt1_args);

        // First attempt: use the preferred workspace if the caller
        // pinned one. Emit `cube_workspace_lease_attempted` *before*
        // the subprocess so the timeline shows what we tried even
        // when cube hangs and never returns.
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::CubeWorkspaceLeaseAttempted, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_cube_repo(&repo.repo_id)
                    .with_cube_invocation(attempt1_repr.clone())
                    .with_details(serde_json::json!({
                        "attempt": 1,
                        "prefer_workspace_id": prefer,
                        "fallback_policy": fallback_policy,
                        "allow_dirty": allow_dirty,
                        "timeout_ms": CUBE_LEASE_TIMEOUT.as_millis() as u64,
                        "excluded_workspace_ids": refused,
                    })),
            )
            .await;

        CUBE_WORKSPACE_LEASE_ATTEMPTS.inc(&self.metrics);
        let first_err = match self
            .invoke_lease(
                repo,
                task,
                (prefer, allow_dirty),
                CUBE_LEASE_TIMEOUT,
                adapter,
                &refused_refs,
            )
            .await
        {
            Ok(lease) => {
                CUBE_WORKSPACE_LEASE_SUCCESS.inc(&self.metrics);
                return Ok(lease);
            }
            Err((reason, err)) => {
                tracing::error!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id,
                    cube_repo_id = %repo.repo_id,
                    prefer = ?prefer,
                    allow_dirty,
                    reason,
                    error = format!("{err:#}"),
                    "cube workspace lease attempt failed"
                );
                let mut details = serde_json::json!({
                    "attempt": 1,
                    "prefer_workspace_id": prefer,
                    "reason": reason,
                    "fallback_policy": fallback_policy,
                    "allow_dirty": allow_dirty,
                    "excluded_workspace_ids": refused,
                });
                augment_details_with_cube_cli_error(&mut details, &err);
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeWorkspaceLeaseFailed, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_error(&err)
                            .with_cube_invocation(attempt1_repr)
                            .with_details(details),
                    )
                    .await;
                err
            }
        };

        // Fallback only kicks in when the first attempt had no workspace
        // preference, OR when prefer_is_soft is true (revision_implementation
        // uses a soft prefer for cache warmth only — losing the preferred
        // workspace is a non-event, not a continuity failure).
        // With a hard prefer (prefer set + prefer_is_soft = false), the
        // caller needs that specific workspace (orphan-resume); silently
        // landing elsewhere would lose local commit state.
        // allow_dirty additionally implies hard-fail: the uncommitted patch
        // lives only in the named workspace, so landing elsewhere is
        // meaningless and must surface an error rather than silently
        // dispatching to a clean workspace.
        // P5 — a resume that loses the workspace race is not automatically
        // doomed. The hard pin exists because the uncommitted work lived ONLY
        // in that workspace; once the engine has captured a recovery patch
        // for the dead execution, that premise is false and the work is
        // reproducible anywhere. Observed failure mode: a fresh dispatch
        // grabbed the pinned workspace seven seconds before the resume, the
        // resume burned its retries against a guaranteed-failing lease, and
        // the item terminalized to `todo` with `blocked_reason: null` — no
        // user-visible signal at all.
        //
        // So: degrade to `any_free` when (and only when) there is a patch to
        // replay. Without one, the hard fail is still correct.
        let recovery_patch = self.recovery_patch_for_resume(execution);
        let patch_rescues_this_resume = allow_dirty && recovery_patch.is_some();
        if prefer.is_some() && (!execution.prefer_is_soft || allow_dirty) && !patch_rescues_this_resume {
            CUBE_WORKSPACE_LEASE_FAILURE.inc(&self.metrics);
            return Err(first_err);
        }
        if patch_rescues_this_resume {
            let (dead_execution_id, patch_path) = recovery_patch.as_ref().expect("checked above");
            tracing::warn!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                worker_id,
                prefer = ?prefer,
                dead_execution_id = %dead_execution_id,
                patch = %patch_path.display(),
                "resume lost its pinned workspace, but a recovery patch exists; \
                 degrading to any free workspace and replaying the patch there",
            );
            self.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::WorkspaceRecovery, DispatchOutcome::Ok, &execution.id)
                        .with_work_item(&execution.work_item_id)
                        .with_worker(worker_id)
                        .with_details(serde_json::json!({
                            "step": "prefer_degraded_to_any_free",
                            "prefer_workspace_id": prefer,
                            "dead_execution_id": dead_execution_id,
                            "recovery_patch": patch_path.display().to_string(),
                        })),
                )
                .await;
        }

        let mut attempt2_args = vec![
            "--json",
            "workspace",
            "lease",
            repo.repo_id.as_str(),
            "--task",
            task,
            "--release-on-setup-failure",
        ];
        for excluded in &refused_refs {
            attempt2_args.extend_from_slice(&["--exclude", excluded]);
        }
        let attempt2_repr = adapter.command_repr(&attempt2_args);

        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::CubeWorkspaceLeaseAttempted, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_cube_repo(&repo.repo_id)
                    .with_cube_invocation(attempt2_repr.clone())
                    .with_details(serde_json::json!({
                        "attempt": 2,
                        "prefer_workspace_id": serde_json::Value::Null,
                        "fallback_policy": "none",
                        "timeout_ms": CUBE_LEASE_TIMEOUT.as_millis() as u64,
                        "fallback_from_prefer": prefer,
                        "excluded_workspace_ids": refused,
                    })),
            )
            .await;

        CUBE_WORKSPACE_LEASE_ATTEMPTS.inc(&self.metrics);
        match self
            .invoke_lease(repo, task, (None, false), CUBE_LEASE_TIMEOUT, adapter, &refused_refs)
            .await
        {
            Ok(lease) => {
                CUBE_WORKSPACE_LEASE_SUCCESS.inc(&self.metrics);
                Ok(lease)
            }
            Err((reason, err)) => {
                tracing::error!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id,
                    cube_repo_id = %repo.repo_id,
                    reason,
                    error = format!("{err:#}"),
                    "cube workspace lease fallback also failed"
                );
                let mut details = serde_json::json!({
                    "attempt": 2,
                    "prefer_workspace_id": serde_json::Value::Null,
                    "reason": reason,
                    "fallback_policy": "none",
                    "fallback_from_prefer": prefer,
                    "excluded_workspace_ids": refused,
                });
                augment_details_with_cube_cli_error(&mut details, &err);
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeWorkspaceLeaseFailed, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_error(&err)
                            .with_cube_invocation(attempt2_repr)
                            .with_details(details),
                    )
                    .await;
                CUBE_WORKSPACE_LEASE_FAILURE.inc(&self.metrics);
                Err(err)
            }
        }
    }

    /// Run one `cube workspace lease` invocation under
    /// [`CUBE_LEASE_TIMEOUT`]. Returns `(reason, error)` so the caller
    /// can label the dispatch event with `"timeout"` vs `"cube_error"`
    /// without re-parsing the message.
    async fn invoke_lease(
        &self,
        repo: &CubeRepoHandle,
        task: &str,
        // (prefer_workspace_id, allow_dirty) — bundled to keep the
        // parameter count under clippy::too_many_arguments.
        lease_opts: (Option<&str>, bool),
        timeout: Duration,
        adapter: &Arc<dyn HostAdapter>,
        exclude_workspace_ids: &[&str],
    ) -> std::result::Result<CubeWorkspaceLease, (&'static str, anyhow::Error)> {
        let (prefer_workspace_id, allow_dirty) = lease_opts;
        match tokio::time::timeout(
            timeout,
            adapter.lease_workspace(
                &repo.repo_id,
                task,
                prefer_workspace_id,
                allow_dirty,
                exclude_workspace_ids,
            ),
        )
        .await
        {
            Ok(Ok(lease)) => Ok(lease),
            Ok(Err(err)) => Err(("cube_error", err)),
            Err(_elapsed) => Err((
                "timeout",
                anyhow!("cube workspace lease timed out after {}s", timeout.as_secs()),
            )),
        }
    }

    /// Record a pre-start failure and either schedule an automatic retry
    /// or surface a permanent failure to the operator.
    ///
    /// Safe-to-retry stages (no worker side effects yet):
    /// `cube_repo_ensure`, `workspace_lease`, `change_create`,
    /// `run_start` (DB-only failure, transaction rolled back).
    ///
    /// Do NOT call this for post-`run_started` failures — those require
    /// `finish_execution_run`.
    /// Shared shape for the two `cube repo ensure` failure arms (error and
    /// timeout): emit a `CubeRepoEnsureFailed` dispatch event carrying the
    /// reproducible cube invocation, then record the pre-start failure. Only
    /// the error, the details payload, and the (attention_kind, attention_title)
    /// tuple differ between the arms.
    async fn emit_ensure_failed_and_record(
        self: &Arc<Self>,
        execution: &WorkExecution,
        worker_id: &str,
        ensure_repr: Option<(String, String)>,
        error: &anyhow::Error,
        details: serde_json::Value,
        attention: (&str, &str),
    ) -> Result<()> {
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::CubeRepoEnsureFailed, DispatchOutcome::Error, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_error(error)
                    .with_cube_invocation(ensure_repr)
                    .with_details(details),
            )
            .await;
        self.record_start_failure(Arc::clone(self), execution, worker_id, None, attention, error)
    }

    fn record_start_failure(
        &self,
        coordinator: Arc<ExecutionCoordinator>,
        execution: &WorkExecution,
        worker_id: &str,
        cube_repo_id: Option<&str>,
        // (attention_kind, attention_title) — bundled to keep the
        // parameter count under clippy::too_many_arguments.
        attention: (&str, &str),
        error: &anyhow::Error,
    ) -> Result<()> {
        let (attention_kind, attention_title) = attention;
        let (execution, run, outcome) = self.work_db.record_pre_start_failure(
            &execution.id,
            worker_id,
            cube_repo_id,
            &error.to_string(),
            &self.pre_start_retry_delays,
        )?;

        match outcome {
            PreStartFailureOutcome::Retry { delay } => {
                tracing::info!(
                    execution_id = %execution.id,
                    run_id = %run.id,
                    worker_id,
                    pre_start_failure_count = execution.pre_start_failure_count,
                    max_retries = self.pre_start_retry_delays.len(),
                    delay_secs = delay.as_secs(),
                    "pre-start failure will retry after backoff"
                );
                // After the backoff window expires, promote the execution
                // back into the ready queue and wake the scheduler. Until
                // then `dispatch_not_before` keeps it invisible to
                // `list_ready_executions`.
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    coordinator.kick();
                });
            }
            PreStartFailureOutcome::PermanentFail => {
                tracing::warn!(
                    execution_id = %execution.id,
                    run_id = %run.id,
                    worker_id,
                    pre_start_failure_count = execution.pre_start_failure_count,
                    error = %error,
                    "recorded execution start failure"
                );

                // Maint task 6 — transient-retry wiring on `dispatch_not_before`:
                // an `automation_triage` execution that exhausts its pre-start
                // retries is the design's `failed_gave_up` terminal state.
                // Finalise the matching `automation_runs` row so the Automations
                // tab shows the occurrence was abandoned (the schedule already
                // advanced past it when the scheduler fired the triage). Until
                // this point the run sat at the pessimistic `failed_will_retry`.
                if execution.kind == ExecutionKind::AutomationTriage
                    && let Err(err) = self.work_db.finalize_automation_triage_run(
                        &execution.id,
                        boss_protocol::AUTOMATION_OUTCOME_FAILED_GAVE_UP,
                        None,
                        Some(&format!(
                            "triage pre-start failed permanently after {} attempt(s): {error}",
                            execution.pre_start_failure_count
                        )),
                    )
                {
                    tracing::warn!(
                        execution_id = %execution.id,
                        ?err,
                        "failed to mark automation run failed_gave_up after permanent triage pre-start failure",
                    );
                }

                // Surface every permanent pre-start failure as a
                // `WorkAttentionItem` so the failure is diagnosable in one
                // bossctl call instead of needing a tracing-log tail.
                let err = format!("{error:#}");
                let attention_body = format!(
                    "Execution `{execution_id}` could not start on worker `{worker_id}` \
                     after {attempts} attempt(s).\n\n\
                     **Error:** {err}\n\n\
                     Inspect `dispatch-events/executions/{execution_id}/dispatch.jsonl` \
                     for the full stage timeline.",
                    execution_id = execution.id,
                    attempts = execution.pre_start_failure_count,
                );
                if let Err(attention_err) = self.work_db.create_attention_item(CreateAttentionItemInput {
                    execution_id: Some(execution.id.clone()),
                    work_item_id: None,
                    kind: attention_kind.to_owned(),
                    status: None,
                    title: attention_title.to_owned(),
                    body_markdown: attention_body,
                    resolved_at: None,
                }) {
                    tracing::error!(
                        ?attention_err,
                        execution_id = %execution.id,
                        "failed to record attention item for execution start failure",
                    );
                }

                // Stop the silent claim → fail → release → re-queue loop
                // (the "waiting for a slot" vs. "failing to start"
                // ambiguity): bounce the work item to Backlog with
                // `autostart` cleared and the failure reason/error stamped
                // directly on the row so the kanban card renders it
                // inline. Guarded on `status IN ('todo', 'active')`, so
                // this is a no-op for review-phase dispatch kinds
                // (`pr_review`, `ci_remediation`, `conflict_resolution`)
                // whose work item sits in `in_review`/`blocked` — bouncing
                // those would erase review context.
                match self
                    .work_db
                    .bounce_dispatch_failed_to_backlog(&execution.work_item_id, attention_kind, &err)
                {
                    Ok(true) => tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        reason = attention_kind,
                        "bounced work item to backlog after permanent pre-start dispatch failure",
                    ),
                    Ok(false) => {}
                    Err(bounce_err) => tracing::error!(
                        ?bounce_err,
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        "failed to bounce work item to backlog after permanent pre-start dispatch failure",
                    ),
                }

                let publisher = self.publisher.clone();
                let execution_id = execution.id.clone();
                let work_item_id = execution.work_item_id.clone();
                let status_str = execution.status.as_str();
                let product_id = match self.work_db.get_work_item(&work_item_id) {
                    Ok(item) => Some(item.product_id().to_string()),
                    Err(err) => {
                        tracing::warn!(
                            ?err,
                            %work_item_id,
                            "failed to resolve product for runtime broadcast"
                        );
                        None
                    }
                };
                tokio::spawn(async move {
                    publisher
                        .publish(&execution_id, &work_item_id, status_str, "execution_start_failed")
                        .await;
                    if let Some(product_id) = product_id {
                        publisher
                            .publish_work_item_changed(&product_id, &work_item_id, "execution_start_failed")
                            .await;
                    }
                });
            }
        }
        Ok(())
    }
}
