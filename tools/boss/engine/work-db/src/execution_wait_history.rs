//! Execution-scoped human waits, independent of task-property auditing.
use super::*;

pub(super) const MIGRATION_ID: &str = "20261103_1800_execution_wait_history";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS execution_wait_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            execution_id TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,
            changed_at INTEGER NOT NULL,
            age_is_lower_bound INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS execution_wait_history_execution
            ON execution_wait_history(execution_id, id);
         INSERT INTO execution_wait_history (execution_id, changed_at, age_is_lower_bound)
            SELECT new_value, CAST(changed_at AS INTEGER), age_is_lower_bound
            FROM task_status_audit a
            WHERE property = 'execution_waiting_human'
              AND EXISTS (SELECT 1 FROM work_executions e WHERE e.id = a.new_value)
            ORDER BY a.id;
         DROP TRIGGER IF EXISTS execution_waiting_human_insert;
         DROP TRIGGER IF EXISTS execution_waiting_human_update;
         DELETE FROM task_status_audit WHERE property = 'execution_waiting_human';
         INSERT INTO execution_wait_history (execution_id, changed_at, age_is_lower_bound)
            SELECT id, strftime('%s','now'), 1 FROM work_executions e
            WHERE status = 'waiting_human' AND NOT EXISTS (
                SELECT 1 FROM execution_wait_history h WHERE h.execution_id = e.id);
         CREATE TRIGGER IF NOT EXISTS execution_wait_history_insert AFTER INSERT ON work_executions
         WHEN NEW.status = 'waiting_human' BEGIN
            INSERT INTO execution_wait_history (execution_id, changed_at)
                VALUES (NEW.id, strftime('%s','now'));
         END;
         CREATE TRIGGER IF NOT EXISTS execution_wait_history_update AFTER UPDATE OF status ON work_executions
         WHEN NEW.status = 'waiting_human' AND OLD.status IS NOT NEW.status BEGIN
            INSERT INTO execution_wait_history (execution_id, changed_at)
                VALUES (NEW.id, strftime('%s','now'));
         END;",
    )?;
    Ok(())
}
