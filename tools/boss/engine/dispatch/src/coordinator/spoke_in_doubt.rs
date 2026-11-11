//! What dispatch does while a spoke host has not said whether it granted a
//! lease or started a worker: park the attempt, tell the operator once the
//! wait is past its bound, and clear the notice when the host answers.
use super::*;

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
             assignment, workspace and persona and is neither failed nor re-dispatched while the \
             dispatcher keeps asking the host about the stored command. This clears by itself once the \
             host answers.\n\n**Last answer:** {err:#}",
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
