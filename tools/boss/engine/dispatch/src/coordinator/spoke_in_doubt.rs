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
             keeps asking the host about the stored command, and about the worker's session if the \
             execution has ended. The item resolves once the host reports the outcome; if the host \
             stays unreachable it stays open.\n\n**Last answer:** {err:#}",
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

    /// Settle executions that ended while their spoke spawn was still in doubt.
    ///
    /// Driven from persisted state (the open in-doubt attention item and the
    /// execution's open assignment), so it works after the task that parked
    /// the spawn is gone: after a cancel, and after an engine restart. For each
    /// one it asks the host about the worker's session. A live worker is
    /// stopped. Once the host reports the session gone, the workspace lease
    /// is released (the release itself still refuses while the stored spawn
    /// is unanswered or settled as uncertain, since it may yet start a
    /// worker), and only then are the persona and live state handed back and
    /// the attention item resolved. Executions that have not ended are left to
    /// the task driving them.
    pub async fn reconcile_ended_in_doubt_spokes(self: &Arc<Self>) {
        let items = match self
            .work_db
            .list_open_attention_items_of_kind(SPOKE_ASSIGNMENT_IN_DOUBT_ATTENTION_KIND)
        {
            Ok(items) => items,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "could not list the in-doubt spoke attention items");
                return;
            }
        };
        for item in items {
            let Some(execution_id) = item.execution_id.as_deref() else {
                continue;
            };
            self.reconcile_ended_in_doubt_spoke(execution_id).await;
        }
    }

    async fn reconcile_ended_in_doubt_spoke(self: &Arc<Self>, execution_id: &str) {
        let execution = match self.work_db.get_execution(execution_id) {
            Ok(execution) if execution.status.is_terminal() => execution,
            Ok(_) => return,
            Err(error) => {
                tracing::warn!(execution_id, error = %format!("{error:#}"), "could not read an execution with an in-doubt spoke");
                return;
            }
        };
        let host_id = match self.work_db.spoke_host_holding_execution(execution_id) {
            Ok(Some(host_id)) => host_id,
            Ok(None) => {
                // Nothing is held on any host any more.
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
        let released = match execution.cube_lease_id.as_deref() {
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
        };
        if !released {
            return;
        }
        if let Some(states) = &self.live_worker_states {
            states.release_slot_for_run(execution_id);
            states.release_persona_for_run(execution_id);
        } else if let Err(error) = self.work_db.release_persona(execution_id) {
            tracing::error!(execution_id, %error, "could not release persona after reconciling an in-doubt spoke");
        }
        self.resolve_spoke_in_doubt_attention(execution_id);
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
