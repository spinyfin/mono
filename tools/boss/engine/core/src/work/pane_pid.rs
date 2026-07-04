//! `work_runs.worker_shell_pid` accessors — the durable, restart-robust pane
//! liveness signal.
//!
//! The macOS app reports a worker pane's shell pid once (via
//! `UpdateWorkerShellPid`, after the libghostty surface attaches). Persisting
//! it onto the run row makes it survive an engine/app restart, which is what
//! lets a DB-driven reconciler
//! ([`crate::lost_workspace_sweep::reconcile_if_execution_dead`]) probe pane
//! liveness after the in-memory `LiveWorkerStateRegistry` has been wiped — the
//! gap that let the 2026-07-03 zombies survive the T2168 fix.

use super::*;

impl WorkDb {
    /// Persist the LOCAL worker-pane shell pid onto the latest `work_runs`
    /// row for `execution_id`.
    ///
    /// Mirrors [`Self::set_run_remote_pid_for_execution`] exactly (latest run
    /// by `created_at DESC, id DESC`). Returns `true` when a row was updated,
    /// `false` when no run exists yet (benign — the pid is informational, not
    /// a spawn precondition; the caller logs and moves on).
    pub fn set_run_worker_shell_pid_for_execution(&self, execution_id: &str, shell_pid: i64) -> Result<bool> {
        let conn = self.connect()?;
        let updated = conn.execute(
            "UPDATE work_runs
             SET worker_shell_pid = ?2
             WHERE id = (
                 SELECT id FROM work_runs
                 WHERE execution_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1
             )",
            params![execution_id, shell_pid],
        )?;
        Ok(updated > 0)
    }

    /// Latest run's `(host_id, worker_shell_pid)` for `execution_id`, resolved
    /// the same way [`Self::latest_run_host_for_execution`] resolves the host
    /// (`ORDER BY created_at DESC, id DESC`). Returns `None` when the
    /// execution has no run yet.
    ///
    /// The pane-liveness reconciler needs both in one read: the host to gate
    /// the local-only pid probe (a `kill(pid, 0)` on the engine host is
    /// meaningless for a remote worker), and the pid to probe. `worker_shell_pid`
    /// is `None` when the pane never reported one — itself a signal (a pane
    /// that never attached), which [`crate::execution_liveness::classify_pane_liveness`]
    /// interprets alongside the execution's age.
    pub fn latest_run_host_and_shell_pid_for_execution(
        &self,
        execution_id: &str,
    ) -> Result<Option<(String, Option<i64>)>> {
        let conn = self.connect()?;
        let row = conn
            .query_row(
                "SELECT host_id, worker_shell_pid FROM work_runs
                 WHERE execution_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
                params![execution_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional()?;
        Ok(row)
    }
}
