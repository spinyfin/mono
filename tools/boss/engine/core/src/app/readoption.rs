//! `ServerState` side of the convergence rule in [`crate::worker_readoption`]:
//! given a worker that has proven itself alive for an execution the engine
//! terminalized, either put the run back under tracking or tear the process
//! down.
//!
//! The policy itself is a pure function in [`crate::worker_readoption`]. This
//! module is the part that needs the engine's collaborators — the DB, the
//! worker pool, the live-state registry, the app session — and it exists as
//! its own submodule so the convergence path can be read end to end without
//! being interleaved with the ordinary pane RPCs in [`super::pane_ops`].

use super::*;

use crate::work::WorkExecution;
use crate::worker_readoption::{ContradictionVerdict, ReapReason, classify_contradiction};

#[async_trait::async_trait]
impl crate::worker_readoption::LiveWorkerConvergence for ServerState {
    async fn converge_live_worker(&self, execution_id: &str, trigger: &str) {
        let Ok(execution) = self.work_db.get_execution(execution_id) else {
            return;
        };
        self.converge_terminal_execution(&execution, trigger).await;
    }
}

impl ServerState {
    /// Resolve a terminal execution whose worker is demonstrably alive:
    /// re-adopt it or reap it, per [`classify_contradiction`].
    ///
    /// `trigger` names the signal that proved liveness (`hook_after_terminal`
    /// from the hook fan-out, `redispatch_guard` from the orphan sweep's
    /// durable-pid probe) and is carried on the dispatch event so a recurrence
    /// is attributable to the detector that caught it.
    ///
    /// Serialized per run by [`ServerState::converging_terminal_runs`]: both
    /// triggers can fire for the same run at once (a worker hooks while the
    /// 60 s sweep is mid-pass), and two concurrent resolutions would race
    /// writes against the same execution row. Returns `"in_flight"` when
    /// another resolution already holds the latch.
    ///
    /// Returns the verdict's stable string for the caller's trace line.
    pub(super) async fn converge_terminal_execution(&self, execution: &WorkExecution, trigger: &str) -> &'static str {
        if !self.begin_terminal_convergence(&execution.id) {
            tracing::debug!(
                run_id = %execution.id,
                trigger,
                "terminal-execution convergence already in flight; skipping",
            );
            return "in_flight";
        }
        let verdict = self.converge_terminal_execution_inner(execution, trigger).await;
        self.end_terminal_convergence(&execution.id);
        verdict
    }

    async fn converge_terminal_execution_inner(&self, execution: &WorkExecution, trigger: &str) -> &'static str {
        let other_live = self
            .work_db
            .get_live_execution_for_work_item(&execution.work_item_id, &execution.id)
            .ok()
            .flatten()
            .map(|live| live.id);
        let verdict = classify_contradiction(&execution.status, other_live.as_deref());
        match verdict {
            ContradictionVerdict::NoContradiction => "no_contradiction",
            ContradictionVerdict::Readopt => {
                self.readopt_live_worker(execution, trigger).await;
                "readopt"
            }
            ContradictionVerdict::Reap { reason } => {
                self.reap_contradicting_worker(execution, trigger, reason, other_live.as_deref())
                    .await;
                "reap"
            }
        }
    }

    /// Claim the right to resolve `run_id`'s contradiction. Returns `false`
    /// when another resolution is already in flight for the same run — see
    /// [`ServerState::converging_terminal_runs`] for why the latch exists.
    pub(super) fn begin_terminal_convergence(&self, run_id: &str) -> bool {
        self.converging_terminal_runs
            .lock()
            .expect("converging_terminal_runs poisoned")
            .insert(run_id.to_owned())
    }

    /// Release the latch taken by [`Self::begin_terminal_convergence`].
    pub(super) fn end_terminal_convergence(&self, run_id: &str) {
        self.converging_terminal_runs
            .lock()
            .expect("converging_terminal_runs poisoned")
            .remove(run_id);
    }

    /// Put a run the engine wrongly declared dead back under tracking.
    ///
    /// Order matters, and it is the reverse of teardown:
    ///
    /// 1. **The DB row first.** Everything else is derived state that a sweep
    ///    can rebuild; the row is what decides whether the work item is
    ///    eligible for re-dispatch. Restoring it first closes the duplicate
    ///    window as early as possible, and if any later step fails the row is
    ///    still correct.
    /// 2. **The pool claim**, keyed to the slot the app actually hosts the
    ///    pane in. This is the `is_live` oracle the re-dispatchers consult, so
    ///    without it the row would still read as re-dispatchable to
    ///    `orphan_sweep` even though its status now says otherwise.
    /// 3. **The live-state entry**, which is what the agent indicator paints
    ///    from and what `bossctl agents list` / `agents stop` resolve against.
    ///
    /// Steps 2 and 3 need a slot id, and the only trustworthy source for it is
    /// the app: the engine's own mapping was cleared by the teardown being
    /// reversed. When the app cannot be asked, re-adoption still completes at
    /// step 1 — degraded (the indicator stays blank until the worker's pane is
    /// re-observed) but convergent in the way that matters, because the row no
    /// longer invites a duplicate dispatch.
    async fn readopt_live_worker(&self, execution: &WorkExecution, trigger: &str) {
        let run_id = execution.id.as_str();
        let prior_status = execution.status.to_string();
        let restored = match self.work_db.readopt_inferred_terminal_execution(run_id, trigger) {
            Ok(restored) => restored,
            Err(err) => {
                tracing::warn!(
                    run_id,
                    error = %format!("{err:#}"),
                    "readopt: could not reverse the inferred terminal status; leaving the row as-is",
                );
                return;
            }
        };

        let shell_pid = crate::durable_liveness::probe_execution_worker(&self.work_db, run_id)
            .alive_pid()
            .unwrap_or(0);
        let slot_id = self.hosted_pane_slot_for_run(run_id).await;
        if let Some(slot_id) = slot_id {
            self.worker_registry.register_run_slot(run_id.to_owned(), slot_id);
            if shell_pid > 0 {
                self.worker_registry.register(shell_pid, run_id.to_owned());
            }
            let worker_id = crate::coordinator::worker_id_for_slot(slot_id);
            if !self.execution_coordinator.reclaim_slot(&worker_id, run_id).await {
                tracing::warn!(
                    run_id,
                    slot_id,
                    "readopt: pool slot could not be re-claimed (occupied by another execution); \
                     the row is restored but re-dispatch protection rests on its status alone",
                );
            }
            let binding =
                self.work_db
                    .get_work_item(&restored.work_item_id)
                    .ok()
                    .map(|item| boss_protocol::WorkItemBinding {
                        work_item_id: restored.work_item_id.clone(),
                        work_item_name: crate::runner::work_item_name(&item).to_owned(),
                        execution_id: run_id.to_owned(),
                    });
            let has_source_automation = matches!(
                self.work_db.source_automation_id_for_work_item(&restored.work_item_id),
                Ok(Some(_))
            );
            let pool = crate::live_worker_state::attributed_pool_label(restored.kind.clone(), has_source_automation);
            // Ask the resolved driver, rather than assume — the same
            // derivation `spawn_flow` makes at spawn time. The driver is
            // durably resolvable from the run's task/product precedence, so
            // there is no reason for re-adoption to guess, and guessing is not
            // cosmetic: `awaiting_input_capable` gates the `WaitingForInput`
            // promotion in `live_worker_state`, so a hardcoded `true` would let
            // `mark_stalled_spawns` paint a re-adopted non-Claude worker as
            // awaiting input — the wrong-indicator class this path exists to
            // end.
            //
            // The spawn-time *model* is not persisted, so the label is the
            // driver's, not a specific model: being vague about the model is
            // cosmetic; being wrong about it would put a false claim on the
            // pane titlebar.
            //
            // The slug not resolving at all (unknown execution, or a row with
            // no task) falls back to the engine default driver, so even the
            // degraded path states some driver's answer rather than a literal.
            let driver = crate::driver_transcript::driver_for_execution(&self.work_db, run_id).or_else(|| {
                crate::driver::DriverRegistry::default()
                    .require(boss_engine_effort::ENGINE_DEFAULT_DRIVER)
                    .ok()
            });
            let model_label = driver
                .as_ref()
                .map(|driver| driver.descriptor().label.to_owned())
                .unwrap_or_else(|| boss_engine_effort::ENGINE_DEFAULT_DRIVER.to_owned());
            let awaiting_input_capable = driver.is_some_and(|driver| {
                driver
                    .capabilities()
                    .provides(crate::driver::Capability::AwaitingInputSignal)
            });
            self.live_worker_states.register_spawn_with_capabilities(
                slot_id,
                run_id.to_owned(),
                model_label,
                shell_pid,
                binding,
                awaiting_input_capable,
                crate::live_worker_state::LiveSpawnRouting::new(pool, restored.kind.as_str()),
            );
            self.broadcast_live_worker_states().await;
        } else {
            tracing::warn!(
                run_id,
                "readopt: the app hosts no pane for this run, so the live-state slot could not be \
                 restored. The execution row is back to live — which is what stops the duplicate \
                 dispatch — but the agent indicator stays blank until the pane is re-observed.",
            );
        }

        if let Ok(item) = self.work_db.get_work_item(&restored.work_item_id) {
            self.publisher
                .publish_work_item_changed(item.product_id(), &restored.work_item_id, "worker_readopted")
                .await;
        }
        self.dispatch_events
            .emit(
                crate::dispatch_events::DispatchEvent::new(
                    crate::dispatch_events::Stage::LiveWorkerReadopted,
                    crate::dispatch_events::Outcome::Ok,
                    run_id,
                )
                .with_work_item(&restored.work_item_id)
                .with_details(serde_json::json!({
                    "trigger": trigger,
                    "prior_status": prior_status,
                    "restored_status": restored.status.to_string(),
                    "shell_pid": shell_pid,
                    "slot_id": slot_id,
                })),
            )
            .await;
    }

    /// Tear down a surviving worker whose execution is terminal for a reason
    /// the engine must not reverse.
    ///
    /// Goes through [`ServerState::release_worker_pane`], which since the
    /// durable-pid fallback landed can reach a worker with no slot mapping —
    /// exactly the shape every worker in this state has, because the teardown
    /// that terminalized it cleared the mapping on its way out.
    async fn reap_contradicting_worker(
        &self,
        execution: &WorkExecution,
        trigger: &str,
        reason: ReapReason,
        other_live: Option<&str>,
    ) {
        let run_id = execution.id.as_str();
        tracing::warn!(
            run_id,
            work_item_id = %execution.work_item_id,
            status = %execution.status,
            reason = reason.as_str(),
            other_live_execution = ?other_live,
            "[engine-reconcile] reaping a worker that outlived its execution — the terminal status \
             is authoritative here, so the surviving process is what must stop",
        );
        let outcome = self.release_worker_pane(run_id).await;
        self.dispatch_events
            .emit(
                crate::dispatch_events::DispatchEvent::new(
                    crate::dispatch_events::Stage::HuskPaneReconcile,
                    crate::dispatch_events::Outcome::Ok,
                    run_id,
                )
                .with_work_item(&execution.work_item_id)
                .with_details(serde_json::json!({
                    "trigger": trigger,
                    "verdict": "reap",
                    "reason": reason.as_str(),
                    "execution_status": execution.status.to_string(),
                    "other_live_execution": other_live,
                    "pane_release_outcome": format!("{outcome:?}"),
                })),
            )
            .await;
    }

    /// The slot the app currently hosts a pane for `run_id` in.
    ///
    /// Deliberately asks the app rather than reading engine bookkeeping: every
    /// caller here is resolving a case where that bookkeeping is known to be
    /// wrong. Best-effort — `None` covers "hosts no pane for this run" and
    /// "could not be asked" alike, and every caller degrades rather than fails.
    ///
    /// Shared with [`ServerState::release_worker_pane`]'s durable-pid fallback:
    /// both need the same answer to the same question for the same reason, so
    /// there is one round-trip shape rather than two.
    pub(super) async fn hosted_pane_slot_for_run(&self, run_id: &str) -> Option<u8> {
        let request = EngineToAppRequest::ListHostedPanes(ListHostedPanesInput {});
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::ListHostedPanes { result: Ok(result) }) => result
                .panes
                .into_iter()
                .find(|pane| pane.run_id == run_id)
                .map(|pane| pane.slot_id),
            other => {
                tracing::debug!(
                    run_id,
                    ?other,
                    "readopt: app could not be asked which slot hosts this run",
                );
                None
            }
        }
    }
}
