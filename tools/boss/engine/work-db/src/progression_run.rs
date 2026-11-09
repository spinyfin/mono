//! Run start for the checkout-free `progression` kind.
use super::*;

impl WorkDb {
    /// Record a `progression` run, its host and its tmux hosting, **without
    /// touching the work item it was sent to diagnose**.
    ///
    /// [`WorkDb::start_execution_run_on_host_with_tmux_hosting`] also advances
    /// the item to `active`, consumes its `autostart` and clears its
    /// `dispatch_failed_*` stamp in the same transaction. For a progression run
    /// that would erase the very evidence of the stuck state the agent is
    /// started to inspect, so this variant writes only the execution and run
    /// rows. The run holds no cube lease: the lease, repo and workspace columns
    /// stay unset and `workspace_path` records the engine-owned scratch
    /// directory it runs in.
    pub fn start_progression_run_on_local_host(
        &self,
        execution_id: &str,
        agent_id: &str,
        scratch_path: &str,
        tmux_hosted: bool,
    ) -> Result<(WorkExecution, WorkRun)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.kind.holds_cube_lease() {
            bail!(
                "execution {execution_id} is a `{}` run; only a progression run may start without a lease",
                execution.kind
            );
        }
        if !execution.status.can_begin_run() {
            bail!(
                "execution {execution_id} is not ready and cannot start a run from status `{}`",
                execution.status
            );
        }

        let now = now_string();
        let updated = tx.execute(
            "UPDATE work_executions
             SET status = 'running',
                 workspace_path = ?2,
                 host_id = 'local',
                 started_at = COALESCE(started_at, ?3),
                 finished_at = NULL,
                 last_error = NULL
             WHERE id = ?1
               AND status IN ('ready', 'claimed')",
            params![execution_id, scratch_path, now],
        )?;
        if updated == 0 {
            let current = query_execution(&tx, execution_id).require("execution", execution_id)?;
            bail!(
                "execution {execution_id} is not ready and cannot start a run from status `{}`",
                current.status
            );
        }

        let run_id = next_id("run");
        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at, host_id, tmux_hosted
             ) VALUES (?1, ?2, ?3, 'active', NULL, NULL, NULL, NULL, ?4, ?4, NULL, 'local', ?5)",
            params![run_id, execution_id, agent_id, now, tmux_hosted],
        )?;

        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let overflow = personas::allocate(&tx, &run_id)?;
        let run = query_run(&tx, &run_id)?.with_context(|| format!("missing run after insert: {run_id}"))?;
        tx.commit()?;
        self.record_persona_overflow(u64::from(overflow));
        Ok((execution, run))
    }
}
