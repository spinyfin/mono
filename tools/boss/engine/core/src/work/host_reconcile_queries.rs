//! `WorkDb` reads for the host-reconcile sweep ([`crate::host_reconcile`]):
//! find the executions bound to a host that has gone offline, and answer
//! whether a single execution's bound host is offline (the heartbeat
//! auto-reap's positive-evidence signal). Kept in their own submodule so
//! the sweep's query surface is cohesive and self-contained.

use super::*;

impl WorkDb {
    /// Non-terminal executions whose latest run landed on a host that is
    /// no longer eligible to run it — the host was disabled (operator
    /// `bossctl hosts disable` or the dispatch-health circuit breaker in
    /// [`WorkDb::record_host_dispatch_failure`]) or removed from the
    /// registry. This is the candidate set for the host-reconcile sweep
    /// ([`crate::host_reconcile`]), which terminalizes each one and lets
    /// the orphan→redispatch machinery re-place its work item on a
    /// still-eligible host — the gap the 2026-07-03 anaplian incident
    /// exposed (disabling the host left its in-flight executions stuck,
    /// heartbeat-erroring forever, with no re-route).
    ///
    /// The join binds each execution to its single most-recent
    /// `work_runs` row; a run whose `host_id = 'local'` is excluded (a
    /// local worker's liveness is judged by the local-filesystem sweeps —
    /// `dead_pid` / `lost_workspace` — not by host registry state, and
    /// `local` is never disabled/removed anyway). "Offline" is either the
    /// host row being absent (`remove_host`) or present with
    /// `enabled = 0` (`set_host_enabled(false)` / auto-disable).
    pub fn list_nonterminal_executions_on_offline_hosts(&self) -> Result<Vec<HostBoundExecution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT e.id, e.work_item_id, e.kind, e.status, e.repo_remote_url, e.cube_repo_id, e.cube_lease_id,
                    e.cube_workspace_id, e.workspace_path, e.priority, e.preferred_workspace_id,
                    e.created_at, e.started_at, e.finished_at,
                    e.pre_start_failure_count, e.dispatch_not_before, e.pr_url, e.pr_head_before, e.prefer_is_soft,
                    e.worker_branch_prefix, e.transient_failure_count, e.allow_dirty, e.branch_naming,
                    r.host_id, r.id
             FROM work_executions e
             JOIN work_runs r ON r.id = (
                 SELECT id FROM work_runs WHERE execution_id = e.id ORDER BY created_at DESC, id DESC LIMIT 1
             )
             WHERE e.status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
               AND r.host_id != 'local'
               AND (
                   NOT EXISTS (SELECT 1 FROM hosts h WHERE h.id = r.host_id)
                   OR EXISTS (SELECT 1 FROM hosts h WHERE h.id = r.host_id AND h.enabled = 0)
               )
             ORDER BY e.created_at ASC, e.id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let execution = map_execution(row)?;
            Ok(HostBoundExecution {
                execution,
                host_id: row.get(23)?,
                run_id: row.get(24)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Whether `execution_id`'s latest run is bound to a host that is
    /// currently offline (disabled or removed) — the positive-evidence
    /// signal the cube-lease heartbeat auto-reap uses to reap a
    /// persistently heartbeat-failing execution without a live `cube
    /// workspace list` confirmation. When the operator (or the circuit
    /// breaker) has declared the host gone, a heartbeat that keeps
    /// erroring is not a transient cube blip to wait out — the worker is
    /// unreachable, so the execution must be terminalized and re-routed
    /// rather than emitting `outcome=error` events forever (the second
    /// half of the 2026-07-03 anaplian incident). Returns `false` when
    /// the run is `local` or its host is still enabled, and when the
    /// execution has no run yet (nothing is bound).
    pub fn execution_bound_host_offline(&self, execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let offline: Option<i64> = conn
            .query_row(
                "SELECT CASE
                        WHEN r.host_id = 'local' THEN 0
                        WHEN NOT EXISTS (SELECT 1 FROM hosts h WHERE h.id = r.host_id) THEN 1
                        WHEN EXISTS (SELECT 1 FROM hosts h WHERE h.id = r.host_id AND h.enabled = 0) THEN 1
                        ELSE 0
                    END
                 FROM work_runs r
                 WHERE r.execution_id = ?1
                 ORDER BY r.created_at DESC, r.id DESC
                 LIMIT 1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()
            .context("execution_bound_host_offline query")?;
        Ok(offline.unwrap_or(0) != 0)
    }
}
