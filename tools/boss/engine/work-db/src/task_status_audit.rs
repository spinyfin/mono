//! Atomic task-state history. The write helper supplies provenance; SQLite
//! captures row values, including bulk updates, inside the writer's transaction.
use super::*;

pub(super) const MIGRATION_ID: &str = "20261029_0314_task_status_audit";
pub(super) const DEFERRED_MIGRATION_ID: &str = "20261112_1900_task_deferred_audit";

/// The task-field changes `boss task show` reports from the ledger: the two
/// flags that hold an item out of automatic dispatch. Each has its own
/// trigger, so every write path leaves a row whether or not it went through
/// [`execute`]; a bare write records `unattributed_write`.
pub(super) const FLAG_PROPERTIES: [&str; 2] = ["deferred", "autostart"];

/// A savepoint also makes callers that otherwise use autocommit atomic.
/// The context is transaction-local in effect: SQLite serializes writers and
/// readers cannot see the context until the savepoint has removed it.
pub fn execute<P: rusqlite::Params>(
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
        // Parking's parked_at trigger reads this table (never the ledger
        // tables) so ALTER TABLE rewrite still works after an upgrade test
        // has dropped the ledger. Missing is the pre-parking schema: try
        // the write and treat "no such table" as absent rather than
        // looking up sqlite_master on every task-status write.
        let parking_ctx = match conn.execute(
            "INSERT OR REPLACE INTO parking_audit_context (singleton, basis, actor) VALUES (1, ?1, ?2)",
            params![basis, actor],
        ) {
            Ok(_) => true,
            Err(err) if super::parking::sqlite_missing_table(&err, "parking_audit_context") => false,
            Err(err) => return Err(err),
        };
        let count = conn.execute(sql, params)?;
        super::parking::flush_parked_at_ledger(conn)?;
        conn.execute("DELETE FROM task_status_audit_context", [])?;
        if parking_ctx {
            conn.execute("DELETE FROM parking_audit_context", [])?;
        }
        Ok(count)
    })();
    if result.is_err() {
        conn.execute_batch("ROLLBACK TO task_status_write")?;
    }
    conn.execute_batch("RELEASE task_status_write")?;
    result
}

pub(super) fn migrate(conn: &Connection) -> Result<()> {
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
        install_property_triggers(conn, property)?;
    }
    Ok(())
}

/// Start recording `tasks.deferred`. Deferring abandons pending executions, so
/// who deferred an item and when it was cleared must be answerable. Items that
/// predate the trigger get one baseline row so the first recorded change has
/// something to be compared against; its age is a lower bound.
pub(super) fn migrate_deferred(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO task_status_audit
            (work_item_id, property, new_value, actor, changed_at, basis, age_is_lower_bound)
         SELECT t.id, 'deferred', CAST(t.deferred AS TEXT), t.last_status_actor,
                strftime('%s','now'), 'unknown_legacy', 1
         FROM tasks t
         WHERE NOT EXISTS (SELECT 1 FROM task_status_audit a
                           WHERE a.work_item_id = t.id AND a.property = 'deferred')",
        [],
    )?;
    install_property_triggers(conn, "deferred")
}

impl WorkDb {
    /// The recorded `deferred` / `autostart` changes of one task, oldest first.
    pub fn list_task_field_audit(&self, work_item_id: &str) -> Result<Vec<boss_protocol::TaskFieldAuditRow>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT property, old_value, new_value, actor, basis, changed_at, age_is_lower_bound
             FROM task_status_audit
             WHERE work_item_id = ?1 AND property IN (?2, ?3)
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![work_item_id, FLAG_PROPERTIES[0], FLAG_PROPERTIES[1]], |row| {
                Ok(boss_protocol::TaskFieldAuditRow {
                    property: row.get(0)?,
                    old_value: row.get(1)?,
                    new_value: row.get(2)?,
                    actor: row.get(3)?,
                    basis: row.get(4)?,
                    changed_at: row.get(5)?,
                    age_is_lower_bound: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// Install the triggers that record every change to `tasks.<property>`.
pub(super) fn install_property_triggers(conn: &Connection, property: &str) -> Result<()> {
    // Repeated failure observations carry new provenance even when the
    // displayed reason is unchanged. Ordinary unchanged updates do not
    // restart the age clock.
    let repeated = if property == "blocked_reason" {
        let bases = super::pr_flow::WorkerFailureBasis::ALL
            .iter()
            .map(|basis| format!("'{}'", basis.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "OR (NEW.blocked_reason = 'worker_failed' AND EXISTS (
                SELECT 1 FROM task_status_audit_context WHERE basis IN ({bases})))"
        )
    } else if property == "dispatch_failed_reason" {
        "OR (NEW.dispatch_failed_reason IS NOT NULL AND NEW.dispatch_failed_at IS NOT OLD.dispatch_failed_at)"
            .to_owned()
    } else {
        String::new()
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
    Ok(())
}
