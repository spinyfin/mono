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
pub(super) const PERIODIC_SWEEP_MIGRATION_ID: &str = "20261122_0630_work_db_periodic_sweep_indexes";

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

fn execution_statuses_sql(terminal: bool) -> String {
    boss_protocol::ExecutionStatus::ALL
        .iter()
        .filter(|status| status.is_terminal() == terminal)
        .map(|status| format!("'{}'", status.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every execution status that is not terminal, as the body of an SQL `IN`
/// list. Unfinished executions are found through the existing status index
/// `work_executions_ready_idx`, which can seek a list of statuses but not
/// `status NOT IN (<terminal>)`. A partial index on that predicate would
/// compete with `work_executions_in_flight_idx`, which has the same one, for
/// the statement that index was built for. Built from the enum so a status
/// added later is included.
pub(crate) fn unfinished_execution_statuses_sql() -> String {
    execution_statuses_sql(false)
}

/// Every terminal execution status, as the body of an SQL `IN` list.
pub(crate) fn terminal_execution_statuses_sql() -> String {
    execution_statuses_sql(true)
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
    apply_with_stat1_guard(conn, INDEXES_SQL, ANALYZE_SQL)
}

pub(super) const PERIODIC_SWEEP_INDEXES_SQL: &str = "
    CREATE INDEX IF NOT EXISTS tasks_positioning_refused_idx
        ON tasks(id)
        WHERE deleted_at IS NULL AND blocked_reason = 'workspace_positioning_refused';
    CREATE INDEX IF NOT EXISTS tasks_open_pr_idx
        ON tasks(id)
        WHERE status NOT IN ('done', 'archived')
          AND deleted_at IS NULL
          AND pr_url IS NOT NULL AND pr_url != '';
    CREATE INDEX IF NOT EXISTS tasks_pr_url_live_non_revision_idx
        ON tasks(pr_url)
        WHERE pr_url IS NOT NULL AND pr_url != ''
          AND deleted_at IS NULL AND kind != 'revision';
    CREATE INDEX IF NOT EXISTS tasks_origin_pr_url_idx
        ON tasks(id)
        WHERE deleted_at IS NULL AND origin_pr_number IS NOT NULL
          AND pr_url IS NOT NULL AND pr_url != '';
    CREATE INDEX IF NOT EXISTS work_executions_preserved_commit_idx
        ON work_executions(id)
        WHERE preserved_commit_id IS NOT NULL;
    CREATE INDEX IF NOT EXISTS work_executions_never_started_prunable_idx
        ON work_executions(work_item_id, created_at, id)
        WHERE started_at IS NULL
          AND status IN ('abandoned', 'failed', 'orphaned', 'cancelled');
    CREATE INDEX IF NOT EXISTS execution_bookmarks_recovered_from_idx
        ON execution_bookmarks(execution_id, recovered_from)
        WHERE recovered_from IS NOT NULL;
    CREATE INDEX IF NOT EXISTS execution_bookmarks_archive_cleanup_idx
        ON execution_bookmarks(execution_id)
        WHERE archive_cleanup_pending = 1;
    CREATE INDEX IF NOT EXISTS work_runs_ephemeral_transcript_idx
        ON work_runs(id)
        WHERE transcript_path IS NOT NULL
          AND (instr(transcript_path, '/boss-grok-homes/') > 0
            OR instr(transcript_path, '/boss-codex-homes/') > 0);";

const PERIODIC_SWEEP_ANALYZE_SQL: &str = "
    ANALYZE tasks_positioning_refused_idx;
    ANALYZE tasks_open_pr_idx;
    ANALYZE tasks_pr_url_live_non_revision_idx;
    ANALYZE tasks_origin_pr_url_idx;
    ANALYZE work_executions_preserved_commit_idx;
    ANALYZE work_executions_never_started_prunable_idx;
    ANALYZE execution_bookmarks_recovered_from_idx;
    ANALYZE execution_bookmarks_archive_cleanup_idx;
    ANALYZE work_runs_ephemeral_transcript_idx;";

pub(super) fn migrate_periodic_sweep(conn: &Connection) -> Result<()> {
    apply_with_stat1_guard(conn, PERIODIC_SWEEP_INDEXES_SQL, PERIODIC_SWEEP_ANALYZE_SQL)
}

fn apply_with_stat1_guard(conn: &Connection, indexes_sql: &str, analyze_sql: &str) -> Result<()> {
    conn.execute_batch(indexes_sql)?;
    // Only analyze the new indexes if `sqlite_stat1` already exists. Running
    // `ANALYZE <index>` on a never-analyzed database would create `sqlite_stat1`
    // and suppress the full `ANALYZE` in `refresh_query_planner_stats`.
    let has_stats: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'sqlite_stat1')",
        [],
        |row| row.get(0),
    )?;
    if has_stats {
        conn.execute_batch(analyze_sql)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn stat_less_upgrade_populates_stat1_for_preexisting_indexes() {
        let (dir, db) = crate::test_support::open_db();
        let path = db.path().to_path_buf();
        let product_id = crate::test_support::create_product(&db);
        let chore_id = crate::test_support::create_active_chore(&db, &product_id, "t");
        let _exec = crate::test_support::create_ready_chore_execution(&db, &chore_id);
        {
            let conn = db.connect().unwrap();
            let created: Vec<String> = super::INDEXES_SQL
                .split("CREATE INDEX IF NOT EXISTS ")
                .skip(1)
                .map(|statement| statement.split_whitespace().next().unwrap().to_owned())
                .collect();
            for index in &created {
                conn.execute_batch(&format!("DROP INDEX IF EXISTS {index}")).unwrap();
            }
            conn.execute_batch("DROP TABLE IF EXISTS sqlite_stat1; DROP TABLE IF EXISTS sqlite_stat4;")
                .unwrap();
            conn.execute("DELETE FROM schema_migrations WHERE id = ?1", [super::MIGRATION_ID])
                .unwrap();
        }
        drop(db);
        let upgraded = crate::WorkDb::open(path).unwrap();
        let conn = upgraded.connect().unwrap();
        let ready_stats: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_stat1 WHERE idx = 'work_executions_ready_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            ready_stats > 0,
            "upgrading a never-analyzed database must still populate sqlite_stat1 for pre-existing indexes"
        );
        drop(dir);
    }
}
