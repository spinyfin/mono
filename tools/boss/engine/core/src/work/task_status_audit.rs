//! Atomic task-state history. The write helper supplies provenance; SQLite
//! captures row values, including bulk updates, inside the writer's transaction.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerFailureBasis {
    NudgeBreaker,
    DeclaredBlocked,
    DeliveredWithoutPr,
    ForeignPrBinding,
    OperatorNo,
}

impl WorkerFailureBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NudgeBreaker => "nudge_breaker",
            Self::DeclaredBlocked => "declared_blocked",
            Self::DeliveredWithoutPr => "delivered_without_pr",
            Self::ForeignPrBinding => "foreign_pr_binding",
            Self::OperatorNo => "operator_no",
        }
    }
}

/// A savepoint also makes callers that otherwise use autocommit atomic.
/// The context is transaction-local in effect: SQLite serializes writers and
/// readers cannot see the context until the savepoint has removed it.
pub(crate) fn execute<P: rusqlite::Params>(
    conn: &Connection,
    basis: &str,
    actor: &str,
    sql: &str,
    params: P,
) -> rusqlite::Result<usize> {
    conn.execute_batch("SAVEPOINT task_status_write")?;
    let result = (|| {
        conn.execute(
            "INSERT INTO task_status_audit_context (basis, actor) VALUES (?1, ?2)",
            params![basis, actor],
        )?;
        let count = conn.execute(sql, params)?;
        conn.execute("DELETE FROM task_status_audit_context", [])?;
        Ok(count)
    })();
    if result.is_err() {
        conn.execute_batch("ROLLBACK TO task_status_write")?;
    }
    conn.execute_batch("RELEASE task_status_write")?;
    result
}

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    if !schema_init::table_has_column(conn, "tasks", "progression_grace_waived_at")? {
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN progression_grace_waived_at TEXT")?;
    }
    if !schema_init::table_has_column(conn, "operator_questions", "stuck_classification")? {
        conn.execute_batch(
            "ALTER TABLE operator_questions ADD COLUMN stuck_classification TEXT NOT NULL DEFAULT 'unknown_legacy'",
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_status_audit (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            work_item_id TEXT NOT NULL,
            property TEXT NOT NULL,
            old_value TEXT,
            new_value TEXT,
            actor TEXT NOT NULL,
            changed_at TEXT NOT NULL,
            basis TEXT NOT NULL,
            age_is_lower_bound INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS task_stuck_daily_snapshots (
            day INTEGER PRIMARY KEY, report_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS task_status_audit_item_property
            ON task_status_audit(work_item_id, property, id);
        CREATE INDEX IF NOT EXISTS task_status_audit_item_event
            ON task_status_audit(work_item_id, id);
        CREATE TABLE IF NOT EXISTS task_status_audit_context (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1) DEFAULT 1,
            basis TEXT NOT NULL, actor TEXT NOT NULL
        );",
    )?;
    super::task_status_backfill::backfill(conn)?;
    for property in ["status", "blocked_reason", "dispatch_failed_reason", "autostart"] {
        // Repeated failure observations carry new provenance even when the
        // displayed reason is unchanged. Ordinary unchanged updates do not
        // restart the age clock.
        let repeated = if property == "blocked_reason" {
            "OR (NEW.blocked_reason = 'worker_failed' AND EXISTS (
                SELECT 1 FROM task_status_audit_context WHERE basis IN
                ('nudge_breaker','declared_blocked','delivered_without_pr','foreign_pr_binding','operator_no')))"
        } else if property == "dispatch_failed_reason" {
            "OR (NEW.dispatch_failed_reason IS NOT NULL AND NEW.dispatch_failed_at IS NOT OLD.dispatch_failed_at)"
        } else {
            ""
        };
        conn.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS task_status_audit_update_{property}
             AFTER UPDATE OF {property} ON tasks
             WHEN OLD.{property} IS NOT NEW.{property} {repeated}
             BEGIN
                INSERT INTO task_status_audit
                    (work_item_id, property, old_value, new_value, actor, changed_at, basis)
                VALUES (NEW.id, '{property}', OLD.{property}, NEW.{property},
                    COALESCE((SELECT actor FROM task_status_audit_context), NEW.last_status_actor),
                    strftime('%s','now'),
                    COALESCE((SELECT basis FROM task_status_audit_context), 'unattributed_write'));
                UPDATE tasks SET progression_grace_waived_at = strftime('%s','now')
                WHERE id = NEW.id AND (
                    ('{property}' = 'blocked_reason' AND NEW.blocked_reason = 'worker_failed'
                     AND (SELECT basis FROM task_status_audit_context) IN ('declared_blocked','nudge_breaker'))
                    OR ('{property}' = 'dispatch_failed_reason' AND NEW.dispatch_failed_reason = 'deliberate_park'));
             END;
             CREATE TRIGGER IF NOT EXISTS task_status_audit_insert_{property}
             AFTER INSERT ON tasks BEGIN
                INSERT INTO task_status_audit
                    (work_item_id, property, old_value, new_value, actor, changed_at, basis)
                VALUES (NEW.id, '{property}', NULL, NEW.{property},
                    COALESCE((SELECT actor FROM task_status_audit_context), NEW.last_status_actor),
                    strftime('%s','now'),
                    COALESCE((SELECT basis FROM task_status_audit_context), 'unattributed_write'));
             END;"
        ))?;
    }
    Ok(())
}
