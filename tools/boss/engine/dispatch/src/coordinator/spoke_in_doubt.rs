//! What dispatch does while a spoke host has not said whether it granted a
//! lease or started a worker: park the attempt, raise a deduplicated attention
//! item once the wait exceeds its bound, and resolve it when the host answers.
use super::*;
use boss_execution_contracts::WorkerLife;

impl ExecutionCoordinator {
    /// Park an attempt whose spoke lease the host has not answered: back to
    /// `ready` behind `dispatch_not_before`, with the failure count untouched,
    /// so nothing is held while the host is silent and the retry budget is not
    /// spent on a question the host has yet to answer. The next pass reuses
    /// the stored lease command, so a late grant is still the run's lease. Once
    /// the assignment has been in doubt past the bounded wait an attention item
    /// is raised (once; later passes refresh it). Returns the error to hand
    /// back to the scheduler.
    pub(super) async fn park_in_doubt_spoke_lease(
        self: &Arc<Self>,
        execution: &WorkExecution,
        err: anyhow::Error,
    ) -> anyhow::Error {
        let timing = self.spoke_in_doubt_timing;
        let note = format!(
            "spoke lease unresolved; parked for {}s: {err:#}",
            timing.lease_park_delay.as_secs()
        );
        match self
            .work_db
            .park_pre_start_attempt(&execution.id, timing.lease_park_delay, &note)
        {
            Ok(true) => {
                let coordinator = Arc::clone(self);
                let delay = timing.lease_park_delay;
                boss_task_scope::spawn(async move {
                    tokio::time::sleep(delay).await;
                    coordinator.kick();
                });
            }
            Ok(false) => {}
            Err(park_err) => tracing::error!(
                execution_id = %execution.id,
                error = %format!("{park_err:#}"),
                "could not park an execution whose spoke lease is in doubt",
            ),
        }
        tracing::warn!(
            execution_id = %execution.id,
            error = %format!("{err:#}"),
            "spoke lease unresolved; attempt parked without spending the retry budget",
        );
        self.raise_spoke_in_doubt_attention(execution, "workspace lease", &err, timing.attention_after);
        err
    }

    /// Park an attempt that could not reach the host holding its in-doubt
    /// assignment (host unreachable, repo not ensurable, host no longer
    /// selectable). Placement is never reconsidered and the retry budget is
    /// untouched; the attempt is retried against the same host and stored
    /// command. Releases the per-repo permit, which the caller may hold.
    pub(super) async fn park_held_spoke_attempt(
        self: &Arc<Self>,
        execution: &WorkExecution,
        err: anyhow::Error,
    ) -> anyhow::Error {
        if self.repo_lease_limiter.release(&execution.id) {
            self.kick();
        }
        self.park_in_doubt_spoke_lease(execution, err).await
    }

    /// File the in-doubt attention item once the assignment has waited past
    /// `after`. Repeated calls refresh one item rather than adding more.
    pub(super) fn raise_spoke_in_doubt_attention(
        &self,
        execution: &WorkExecution,
        what: &str,
        err: &anyhow::Error,
        after: Duration,
    ) {
        let age = match self.work_db.spoke_assignment_age_ms(&execution.id) {
            Ok(Some(age)) => Duration::from_millis(age as u64),
            Ok(None) => return,
            Err(read_err) => {
                tracing::warn!(execution_id = %execution.id, error = %format!("{read_err:#}"), "could not read the spoke assignment age");
                return;
            }
        };
        if age < after {
            return;
        }
        let body = format!(
            "The spoke host has not confirmed the {what} for execution `{execution_id}` for {secs}s. \
             The host may have granted the lease or started the worker, so the execution keeps its \
             assignment, workspace and persona and is neither failed nor re-dispatched. The dispatcher \
             keeps asking the host about the stored command from persisted assignment state, including \
             after an engine restart and while the spawn is still running. If the execution has ended it \
             also asks about the worker's session. The item resolves once the host reports the outcome \
             and any granted lease is released; if the host stays unreachable it stays open.\n\n\
             **Last answer:** {err:#}",
            execution_id = execution.id,
            secs = age.as_secs(),
        );
        if let Err(attention_err) = self.work_db.upsert_execution_attention(
            &execution.id,
            SPOKE_ASSIGNMENT_IN_DOUBT_ATTENTION_KIND,
            "Spoke host has not confirmed the assignment",
            &body,
        ) {
            tracing::error!(
                execution_id = %execution.id,
                error = %format!("{attention_err:#}"),
                "failed to record the in-doubt spoke attention item",
            );
        }
    }

    /// Reconcile spoke leases and spawns that are still in doubt.
    ///
    /// Driven from persisted open assignments and stored command state, plus
    /// any open in-doubt attention item, so a parked spawn is still asked
    /// about after an engine restart: there is then no live `run_execution`
    /// task, and there may not yet be an attention item. A still-running
    /// execution with no driver is resumed onto the stored spawn (Alive
    /// continues the launch path; definitive evidence fails it). An ended
    /// execution is asked about the worker, stopped if alive, and released
    /// only once the spawn and any granted lease have a definitive answer.
    pub async fn reconcile_ended_in_doubt_spokes(self: &Arc<Self>) {
        let mut ids = std::collections::BTreeSet::new();
        match self
            .work_db
            .list_open_attention_items_of_kind(SPOKE_ASSIGNMENT_IN_DOUBT_ATTENTION_KIND)
        {
            Ok(items) => {
                for item in items {
                    if let Some(execution_id) = item.execution_id {
                        ids.insert(execution_id);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "could not list the in-doubt spoke attention items");
            }
        }
        match self.work_db.list_in_doubt_spoke_execution_ids() {
            Ok(open) => ids.extend(open),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "could not list in-doubt spoke assignments");
            }
        }
        if ids.is_empty() {
            return;
        }
        let timeout = self.spoke_in_doubt_timing.sweep_item_timeout;
        let mut set = tokio::task::JoinSet::new();
        for execution_id in ids {
            while set.len() >= IN_DOUBT_SWEEP_CONCURRENCY {
                let _ = set.join_next().await;
            }
            let coordinator = Arc::clone(self);
            boss_task_scope::spawn_into(&mut set, async move {
                if tokio::time::timeout(timeout, coordinator.reconcile_in_doubt_spoke(&execution_id))
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        execution_id,
                        "in-doubt spoke reconcile timed out; will retry on the next pass"
                    );
                }
            });
        }
        while set.join_next().await.is_some() {}
    }

    /// Fire the in-doubt sweep without waiting for it. A second call while a
    /// pass is running is a no-op, so the scheduler heartbeat can kick on its
    /// own interval even when a host is silent.
    pub fn spawn_in_doubt_reconcile(self: &Arc<Self>) {
        if self.in_doubt_sweep_active.swap(true, Ordering::AcqRel) {
            return;
        }
        let coordinator = Arc::clone(self);
        boss_task_scope::spawn(async move {
            coordinator.reconcile_ended_in_doubt_spokes().await;
            coordinator.in_doubt_sweep_active.store(false, Ordering::Release);
        });
    }

    async fn reconcile_in_doubt_spoke(self: &Arc<Self>, execution_id: &str) {
        let execution = match self.work_db.get_execution(execution_id) {
            Ok(execution) => execution,
            Err(error) => {
                tracing::warn!(execution_id, error = %format!("{error:#}"), "could not read an execution with an in-doubt spoke");
                return;
            }
        };
        if execution.status.is_live() {
            if self.is_driving_run(execution_id) {
                return;
            }
            // Only a spawn still awaiting its answer is resumed: a worker whose
            // launch already completed must not have its launch path re-driven.
            match self.work_db.active_run_ids_for_execution(execution_id) {
                Ok(runs) if runs.is_empty() => return,
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(execution_id, error = %format!("{error:#}"), "could not read the execution's runs");
                    return;
                }
            }
            match self.resume_pane_spawn_for_running_execution(&execution).await {
                Ok(()) => {}
                Err(error) => tracing::warn!(
                    execution_id,
                    error = %format!("{error:#}"),
                    "could not resume a running in-doubt spoke spawn"
                ),
            }
            return;
        }
        if !execution.status.is_terminal() {
            // Ready/claimed parked leases stay with the dispatch loop, which
            // retries the stored command without spending the pre-start budget.
            return;
        }
        let host_id = match self.work_db.spoke_host_holding_execution(execution_id) {
            Ok(Some(host_id)) => host_id,
            Ok(None) => {
                self.resolve_spoke_in_doubt_attention(execution_id);
                return;
            }
            Err(error) => {
                tracing::warn!(execution_id, error = %format!("{error:#}"), "could not read the in-doubt spoke assignment");
                return;
            }
        };
        let host = match self.work_db.get_host(&host_id) {
            Ok(Some(host)) => host,
            _ => return,
        };
        let adapter = match self
            .host_adapter_provider
            .adapter_for_execution(&host, execution_id)
            .await
        {
            Ok(adapter) => adapter,
            Err(error) => {
                tracing::warn!(execution_id, host_id = %host_id, error = %format!("{error:#}"), "no adapter to reconcile the in-doubt spoke with");
                return;
            }
        };
        let assignment = match self.work_db.spoke_assignment_for_execution(execution_id) {
            Ok(Some(assignment)) => assignment,
            Ok(None) => {
                self.resolve_spoke_in_doubt_attention(execution_id);
                return;
            }
            Err(error) => {
                tracing::warn!(execution_id, error = %format!("{error:#}"), "could not read the in-doubt spoke assignment");
                return;
            }
        };
        let has_spawn = match self
            .work_db
            .assignment_has_stored_spawn(&assignment.host_id, &assignment.run_id)
        {
            Ok(has_spawn) => has_spawn,
            Err(error) => {
                tracing::warn!(execution_id, error = %format!("{error:#}"), "could not read the stored spawn");
                return;
            }
        };
        let released = if has_spawn {
            match adapter.observe_worker(execution_id).await {
                Ok(Some(pane)) if !matches!(pane.life, WorkerLife::Exited | WorkerLife::Missing) => {
                    if pane.life == WorkerLife::Alive {
                        match adapter.stop_worker(execution_id, Duration::ZERO).await {
                            Ok(outcome) => tracing::warn!(
                                execution_id,
                                ?outcome,
                                "stopped the worker an ended execution left on its spoke host"
                            ),
                            Err(error) => {
                                tracing::warn!(execution_id, error = %format!("{error:#}"), "could not stop the worker an ended execution left on its spoke host")
                            }
                        }
                    }
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(execution_id, error = %format!("{error:#}"), "could not ask the spoke host about an ended execution's worker");
                    return;
                }
            }
            let execution = match self.work_db.get_execution(execution_id) {
                Ok(execution) => execution,
                Err(_) => return,
            };
            match execution.cube_lease_id.as_deref() {
                Some(lease_id) => matches!(
                    self.lifecycle
                        .apply_proven_workspace_release(
                            &execution,
                            lease_id,
                            TerminalCause::ProvenStopped,
                            adapter.as_ref()
                        )
                        .await,
                    LeaseReleaseOutcome::Released | LeaseReleaseOutcome::NoLease
                ),
                None => true,
            }
        } else {
            match self.complete_pending_lease(&execution, adapter.as_ref()).await {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    tracing::warn!(execution_id, error = %format!("{error:#}"), "could not complete a pending in-doubt lease");
                    return;
                }
            }
            let execution = match self.work_db.get_execution(execution_id) {
                Ok(execution) => execution,
                Err(_) => return,
            };
            match execution.cube_lease_id.as_deref() {
                Some(lease_id) => matches!(
                    self.lifecycle
                        .release_lease_after_proven_never_started(&execution, lease_id, adapter.as_ref())
                        .await,
                    LeaseReleaseOutcome::Released | LeaseReleaseOutcome::NoLease
                ),
                None => match self
                    .work_db
                    .current_lease_command(&assignment.host_id, &assignment.run_id)
                {
                    Ok(Some(stored)) => stored.result.is_some(),
                    Ok(None) => true,
                    Err(error) => {
                        tracing::warn!(execution_id, error = %format!("{error:#}"), "could not read the stored lease");
                        return;
                    }
                },
            }
        };
        if !released {
            return;
        }
        self.release_in_doubt_persona(execution_id);
        self.resolve_spoke_in_doubt_attention(execution_id);
    }

    /// `Ok(true)` when the stored lease is settled and any grant is bound on
    /// the execution (ready to release). `Ok(false)` when it is still unanswered
    /// or settled-uncertain: keep the item open.
    async fn complete_pending_lease(
        self: &Arc<Self>,
        execution: &WorkExecution,
        adapter: &dyn HostAdapter,
    ) -> anyhow::Result<bool> {
        if execution.cube_lease_id.is_some() {
            return Ok(true);
        }
        match adapter.complete_in_doubt_lease().await {
            Ok(Some(lease)) => {
                self.persist_acquired_lease(execution, &lease);
                Ok(true)
            }
            Ok(None) => Ok(true),
            Err(error) if waitable_spoke_lease(&error).is_some() => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn release_in_doubt_persona(&self, execution_id: &str) {
        if let Some(states) = &self.live_worker_states {
            states.release_slot_for_run(execution_id);
            states.release_persona_for_run(execution_id);
        } else if let Err(error) = self.work_db.release_persona(execution_id) {
            tracing::error!(execution_id, %error, "could not release persona after reconciling an in-doubt spoke");
        }
    }

    pub(super) fn begin_driving_run(&self, execution_id: &str) -> bool {
        self.driving_runs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .ids
            .insert(execution_id.to_owned())
    }

    pub(super) fn store_driving_handle(&self, execution_id: &str, handle: tokio::task::AbortHandle) {
        self.driving_runs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .handles
            .insert(execution_id.to_owned(), handle);
    }

    pub(super) fn forget_driving_run(&self, execution_id: &str) {
        let mut runs = self.driving_runs.lock().unwrap_or_else(|poison| poison.into_inner());
        runs.ids.remove(execution_id);
        runs.handles.remove(execution_id);
    }

    fn is_driving_run(&self, execution_id: &str) -> bool {
        self.driving_runs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .ids
            .contains(execution_id)
    }

    /// Stop every in-flight `run_execution` task. Tests use this to simulate an
    /// engine restart: the next coordinator has an empty driving set.
    pub fn abort_driving_runs(&self) {
        let mut runs = self.driving_runs.lock().unwrap_or_else(|poison| poison.into_inner());
        runs.ids.clear();
        for (_, handle) in runs.handles.drain() {
            handle.abort();
        }
    }

    /// Drop the in-doubt flag once the spawn has a definitive answer. Returns
    /// whether the flag was set.
    pub(super) fn clear_spoke_in_doubt(&self, execution_id: &str) -> bool {
        let assignment = match self.work_db.spoke_assignment_for_execution(execution_id) {
            Ok(Some(assignment)) => assignment,
            Ok(None) => return false,
            Err(error) => {
                tracing::warn!(execution_id, error = %format!("{error:#}"), "could not read the spoke assignment to clear its in-doubt flag");
                return false;
            }
        };
        match self.work_db.spoke_execution_in_doubt(execution_id) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(error) => {
                tracing::warn!(execution_id, error = %format!("{error:#}"), "could not read the spoke in-doubt flag");
                return false;
            }
        }
        if let Err(error) = self
            .work_db
            .set_spoke_in_doubt(execution_id, assignment.assignment_generation, false)
        {
            tracing::warn!(execution_id, error = %format!("{error:#}"), "could not clear the spoke in-doubt flag");
            return false;
        }
        true
    }

    /// The host answered: whatever was in doubt is settled.
    pub(super) fn resolve_spoke_in_doubt_attention(&self, execution_id: &str) {
        if let Err(error) = self
            .work_db
            .resolve_attention_kind_for_execution(execution_id, SPOKE_ASSIGNMENT_IN_DOUBT_ATTENTION_KIND)
        {
            tracing::warn!(execution_id, error = %format!("{error:#}"), "could not resolve the in-doubt spoke attention item");
        }
    }
}

pub(super) fn waitable_spoke_lease(err: &anyhow::Error) -> Option<&LeaseOutcomeUncertain> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<LeaseOutcomeUncertain>())
}

/// Whether the execution has been cancelled or finished. A failed read is not
/// an answer: callers must not treat it as a cancellation, because that would
/// release state a worker may still be using.
pub(super) fn execution_no_longer_runnable(work_db: &WorkDb, execution_id: &str) -> Result<bool> {
    Ok(work_db.get_execution(execution_id)?.status.is_terminal())
}
