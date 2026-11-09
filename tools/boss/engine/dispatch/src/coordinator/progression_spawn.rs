//! Checkout-free ("leaseless") spawn of a `progression` agent. Part of the
//! `coordinator` module split; see [`super`] for the struct and shared types.
//!
//! A progression run diagnoses one stuck work item. It holds no cube
//! workspace lease and no code checkout, so it bypasses the whole
//! repo-ensure → lease → position → change → bookmark pipeline in
//! `schedule_execution` and instead:
//!
//! - runs on the local host in an engine-owned scratch directory
//!   ([`boss_execution_contracts::progression_scratch`]), removed on teardown;
//! - records the run without advancing, clearing or otherwise touching the item
//!   (`WorkDb::start_progression_run_on_local_host`), because the item's stuck
//!   state is the evidence the agent is sent to read;
//! - skips the gating-prerequisite check, since a progression run on a
//!   `blocked: dependency` item must still spawn;
//! - never bounces the item to backlog or demotes it when the spawn fails.
//!
//! The shared guards (dispatch pause, claim, double-spawn) run in
//! `schedule_execution` before it branches here.
use super::*;
use boss_execution_contracts::progression_scratch;

impl ExecutionCoordinator {
    /// Start the progression agent for `execution` on a claimed `worker_id`.
    pub(super) async fn schedule_progression_execution(
        self: &Arc<Self>,
        execution: &WorkExecution,
        worker_id: &str,
    ) -> Result<()> {
        let work_item = match self
            .resolve_execution_work_item(execution)
            .with_context(|| format!("failed to resolve work item {}", execution.work_item_id))
        {
            Ok(work_item) => work_item,
            Err(err) => {
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
                )
                .await?;
                return Err(err);
            }
        };

        // Progression never runs on a remote host: it needs the engine's own
        // tmux hosting and the local scratch directory.
        let adapter = self.host_adapter();
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_details(serde_json::json!({ "host_id": "local", "leaseless": true })),
            )
            .await;

        let scratch = match progression_scratch::create_scratch_dir(&execution.id) {
            Ok(scratch) => scratch,
            Err(err) => {
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    None,
                    (
                        "progression_scratch_failed",
                        "Could not create the progression scratch directory",
                    ),
                    &err,
                )
                .await?;
                return Err(err);
            }
        };
        self.pool_for_worker_id(worker_id).mark_working(worker_id).await;

        match self.work_db.start_progression_run_on_local_host(
            &execution.id,
            worker_id,
            &scratch.display().to_string(),
            true,
        ) {
            Ok((execution, run)) => {
                tracing::info!(
                    execution_id = %execution.id,
                    run_id = %run.id,
                    worker_id,
                    work_item_id = %execution.work_item_id,
                    scratch = %scratch.display(),
                    "started progression run (no cube lease, item untouched)"
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::RunStarted, DispatchOutcome::Ok, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_details(serde_json::json!({
                                "run_id": run.id,
                                "execution_kind": execution.kind.as_str(),
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
                let worker_id_owned = worker_id.to_owned();
                let coordinator = self.clone();
                boss_task_scope::spawn(async move {
                    coordinator
                        .run_progression_execution(execution, run, work_item, worker_id_owned, scratch, adapter)
                        .await;
                });
                Ok(())
            }
            Err(err) => {
                if let Err(remove_err) = progression_scratch::remove_scratch_dir(&execution.id) {
                    tracing::warn!(execution_id = %execution.id, ?remove_err, "could not remove progression scratch dir");
                }
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::RunStarted, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_error(&err),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    None,
                    ("execution_run_start_failed", "`start_execution_run` failed"),
                    &err,
                )
                .await?;
                Err(err)
            }
        }
    }

    /// Spawn the progression pane and record how the dispatch/spawn run ended.
    /// The lease-free counterpart of `run_execution`: no heartbeat, no
    /// workspace release, and a failed spawn leaves the work item alone.
    pub(super) async fn run_progression_execution(
        self: Arc<Self>,
        execution: WorkExecution,
        run: WorkRun,
        work_item: WorkItem,
        worker_id: String,
        scratch: PathBuf,
        adapter: Arc<dyn HostAdapter>,
    ) {
        let streak_key = self.pre_start_streak_key(&execution, &worker_id);
        let run_outcome = adapter
            .spawn_worker(&worker_id, &execution, &work_item, scratch.as_path(), None)
            .await;
        // A live pane keeps its pool slot until `release_worker_pane` tears it
        // down; only a spawn that produced no pane frees the slot here.
        let defer_pool_slot_release = matches!(&run_outcome, Ok(outcome) if outcome.slot_id.is_some());

        match run_outcome {
            Ok(outcome) if outcome.wait_state == RunWaitState::CancelledDuringSpawn => {
                self.lifecycle
                    .teardown_driver_workspace(
                        &execution.id,
                        Some(scratch.as_path()),
                        TeardownReason::CancelledDuringSpawn,
                    )
                    .await;
            }
            Ok(outcome) => {
                self.pre_start_streaks.record_success(&streak_key);
                let run = match outcome.slot_id {
                    Some(slot_id) => self
                        .work_db
                        .set_run_agent_id(&run.id, &worker_id_for_slot(slot_id))
                        .unwrap_or(run),
                    None => run,
                };
                if let Some(spawn) = outcome.spawn_config.as_ref()
                    && let Err(err) = self.work_db.record_execution_launch_config(
                        &execution.id,
                        &spawn.driver,
                        &spawn.model,
                        spawn.effort_level,
                    )
                {
                    tracing::error!(?err, execution_id = %execution.id, "failed to persist progression launch configuration");
                }
                let release_workspace = outcome.wait_state.release_workspace();
                if release_workspace {
                    self.lifecycle
                        .teardown_driver_workspace(&execution.id, Some(scratch.as_path()), TeardownReason::RunCompleted)
                        .await;
                }
                match self.work_db.finish_execution_run(
                    FinishExecutionRunInput::builder()
                        .execution_id(&execution.id)
                        .run_id(&run.id)
                        .execution_status(outcome.wait_state.execution_status())
                        .run_status("completed")
                        .maybe_result_summary(outcome.result_summary.as_deref())
                        .build(),
                ) {
                    Ok((execution, run, _)) => {
                        tracing::info!(
                            execution_id = %execution.id,
                            run_id = %run.id,
                            worker_id = %worker_id,
                            execution_status = %execution.status,
                            "progression run completed (spawn finalized; execution parks at its wait state)"
                        );
                        self.publisher
                            .publish(
                                &execution.id,
                                &execution.work_item_id,
                                execution.status.as_str(),
                                "execution_run_completed",
                            )
                            .await;
                    }
                    Err(err) => {
                        tracing::error!(?err, execution_id = %execution.id, "failed to record progression completion")
                    }
                }
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::PaneSpawned, DispatchOutcome::Ok, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(&worker_id)
                            .with_details(serde_json::json!({
                                "run_id": run.id,
                                "slot_id": slot_id_from_worker_id(&worker_id),
                            })),
                    )
                    .await;
            }
            Err(err) => {
                let err_detail = format!("{err:#}");
                tracing::error!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    run_id = %run.id,
                    worker_id = %worker_id,
                    error = %err_detail,
                    "progression spawn aborted; tearing down (the work item is left untouched)",
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::SpawnFailed, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(&worker_id)
                            .with_error(&err)
                            .with_details(serde_json::json!({ "run_id": run.id })),
                    )
                    .await;
                let slot_busy = slot_busy_occupant(&err).is_some();
                if !slot_busy {
                    self.pre_start_streaks.record_failure(
                        streak_key,
                        &execution.id,
                        &err_detail,
                        boss_engine_utils::epoch_time::now_epoch_secs(),
                    );
                }
                self.lifecycle
                    .teardown_driver_workspace(&execution.id, Some(scratch.as_path()), TeardownReason::SpawnFailed)
                    .await;
                // No demotion, no backlog bounce, no attention item on the
                // item: the run row alone records the failure.
                match self.work_db.finish_execution_run(
                    FinishExecutionRunInput::builder()
                        .execution_id(&execution.id)
                        .run_id(&run.id)
                        .execution_status(ExecutionStatus::Failed)
                        .run_status("failed")
                        .error_text(err_detail.as_str())
                        .increment_pre_start_failure_count(!slot_busy)
                        .record_last_error(true)
                        .build(),
                ) {
                    Ok((execution, _, _)) => {
                        self.publisher
                            .publish(
                                &execution.id,
                                &execution.work_item_id,
                                execution.status.as_str(),
                                "execution_run_failed",
                            )
                            .await;
                    }
                    Err(record_err) => {
                        tracing::error!(?record_err, execution_id = %execution.id, "failed to record progression spawn failure")
                    }
                }
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::PaneSpawned, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(&worker_id)
                            .with_error(&err),
                    )
                    .await;
            }
        }

        if !defer_pool_slot_release {
            if let Some(states) = &self.live_worker_states {
                states.release_slot_for_run(&execution.id);
                states.release_persona_for_run(&execution.id);
            } else if let Err(error) = self.work_db.release_persona(&execution.id) {
                tracing::error!(execution_id = %execution.id, %error, "could not release persona after progression cleanup");
            }
            self.release_worker_and_kick(&worker_id, None).await;
        }
    }
}
