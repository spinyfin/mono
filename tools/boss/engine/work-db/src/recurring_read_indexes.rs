//! Indexes for the reads the engine repeats on a timer.
//!
//! The scheduler beat, the merge poller and the once-a-minute sweeps each
//! look for a handful of live rows: tasks in one open status, executions
//! that have not finished, undecided proposals, runs on a remote host.
//! Without an index on the column that says "live", each of those reads
//! walks its whole table on every tick, and for `tasks` and
//! `worker_proposals` follows every row's overflow chain as well, because
//! the columns being tested are stored after the description or payload.
//! Summed over the timers, an idle engine would re-read its database several
//! times a minute.
//!
//! Five of the seven are partial: they hold only the live rows, so they stay
//! a few pages however much history accumulates. A partial index can only
//! serve a statement whose `WHERE` repeats its predicate, literal for
//! literal, so keep the two in step. Where the planner still prefers a wider
//! index (it estimates an empty partial index at the size of its table), the
//! statement names the index with `INDEXED BY`.
//!
//! See `tools/boss/docs/work-db-read-volume.md` for the measurements and the
//! read-budget test that guards these reads.
use super::{Connection, Result};

pub(super) const MIGRATION_ID: &str = "20261121_1800_recurring_read_indexes";

/// Partial index of live rows with a durable merge-queue projection, so the
/// merge poller's per-pass membership read follows the live queue.
pub(super) const MERGE_QUEUE_INDEX_MIGRATION_ID: &str = "20261122_1600_tasks_merge_queue_live_idx";

pub(super) const MERGE_QUEUE_INDEX_SQL: &str = "
    CREATE INDEX IF NOT EXISTS tasks_merge_queue_live_idx
        ON tasks(id)
        WHERE merge_queue_state IS NOT NULL
          AND status NOT IN ('done', 'archived')
          AND deleted_at IS NULL;";

pub(super) fn migrate_merge_queue_index(conn: &Connection) -> Result<()> {
    conn.execute_batch(MERGE_QUEUE_INDEX_SQL)?;
    conn.execute_batch("ANALYZE tasks_merge_queue_live_idx;")?;
    Ok(())
}

/// Every execution status that is not terminal, as the body of an SQL `IN`
/// list. Unfinished executions are found through the existing status index
/// `work_executions_ready_idx`, which can seek a list of statuses but not
/// `status NOT IN (<terminal>)`. A partial index on that predicate would
/// compete with `work_executions_in_flight_idx`, which has the same one, for
/// the statement that index was built for. Built from the enum so a status
/// added later is included.
pub(crate) fn unfinished_execution_statuses_sql() -> String {
    boss_protocol::ExecutionStatus::ALL
        .iter()
        .filter(|status| !status.is_terminal())
        .map(|status| format!("'{}'", status.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) const INDEXES_SQL: &str = "
    CREATE INDEX IF NOT EXISTS tasks_status_idx
        ON tasks(status, deleted_at);
    CREATE INDEX IF NOT EXISTS tasks_open_idx
        ON tasks(status, deleted_at)
        WHERE status NOT IN ('done', 'archived');
    CREATE INDEX IF NOT EXISTS tasks_dispatch_failed_idx
        ON tasks(id)
        WHERE dispatch_failed_reason IS NOT NULL;
    CREATE INDEX IF NOT EXISTS work_executions_finished_at_idx
        ON work_executions(CAST(finished_at AS INTEGER));
    CREATE INDEX IF NOT EXISTS work_executions_terminal_lease_idx
        ON work_executions(created_at, id)
        WHERE cube_lease_id IS NOT NULL
          AND status IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned');
    CREATE INDEX IF NOT EXISTS worker_proposals_undecided_idx
        ON worker_proposals(kind, created_at, id)
        WHERE state = 'proposed';
    CREATE INDEX IF NOT EXISTS work_runs_local_shell_ended_idx
        ON work_runs(CAST(COALESCE(finished_at, created_at) AS INTEGER))
        WHERE host_id = 'local' AND shell_pid > 0;";

/// Statistics for the new indexes only: each is read once, and the planner
/// then knows how few rows a live status selects without waiting for the
/// next whole-database `ANALYZE`.
const ANALYZE_SQL: &str = "
    ANALYZE tasks_status_idx;
    ANALYZE tasks_open_idx;
    ANALYZE tasks_dispatch_failed_idx;
    ANALYZE work_executions_finished_at_idx;
    ANALYZE work_executions_terminal_lease_idx;
    ANALYZE worker_proposals_undecided_idx;
    ANALYZE work_runs_local_shell_ended_idx;";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(INDEXES_SQL)?;
    conn.execute_batch(ANALYZE_SQL)?;
    Ok(())
}
