//! Atomic task-state history. The write helper supplies provenance; SQLite
//! captures row values, including bulk updates, inside the writer's transaction.
use super::*;

pub(super) const MIGRATION_ID: &str = "20261029_0314_task_status_audit";
pub(super) const DEFERRED_MIGRATION_ID: &str = "20261112_1900_task_deferred_audit";
pub(super) const EXECUTION_ABANDON_MIGRATION_ID: &str = "20261118_1530_execution_abandon_audit";

/// `task_status_audit.property` of a `work_executions` transition to
/// `abandoned`. `old_value` is the execution's previous status, `new_value` is
/// `abandoned`, and `execution_id` names the row (which retention may later
/// delete; the ledger row stays).
pub(crate) const EXECUTION_ABANDONED_PROPERTY: &str = "execution_abandoned";

/// Recorded in `reason_code` and `basis` for a write that reached the ledger
/// without going through a helper in this module.
pub(crate) const UNATTRIBUTED_WRITE: &str = "unattributed_write";

/// Why an execution was abandoned. `code` is the stable, machine-readable
/// reason (never reworded once shipped); `path` names the writer; `detail` is
/// human-readable diagnostic text that may be reworded.
#[derive(Clone, Copy)]
pub(crate) struct AbandonCause<'a> {
    pub code: &'a str,
    pub path: &'a str,
    pub detail: &'a str,
    pub actor: &'a str,
}

impl<'a> AbandonCause<'a> {
    const fn engine(code: &'a str, path: &'a str, detail: &'a str) -> Self {
        Self {
            code,
            path,
            detail,
            actor: "engine",
        }
    }

    /// The same cause attributed to the caller's actor, such as an explicit deferral.
    pub(crate) const fn by(self, actor: &'a str) -> Self {
        Self { actor, ..self }
    }
}

/// Every abandonment cause, one per writer. Shared helpers take the cause of
/// their caller so two callers of one helper never share a code.
pub(crate) mod abandon_cause {
    use super::AbandonCause;

    pub(crate) const GHOST_ACTIVE_DEMOTED: AbandonCause<'static> = AbandonCause::engine(
        "ghost_active_demoted",
        "heal_ghost_active_chores",
        "item was active with no run ever started; pending executions withdrawn while demoting it to todo",
    );
    pub(crate) const CLOSED_ITEM_STRANDED: AbandonCause<'static> = AbandonCause::engine(
        "closed_item_stranded",
        "abandon_stranded_executions_on_closed_work_items",
        "dispatchable execution outlived its closed or deleted work item",
    );
    pub(crate) const REDUNDANT_DUPLICATE: AbandonCause<'static> = AbandonCause::engine(
        "redundant_duplicate",
        "mark_execution_redundant",
        "double-spawn guard discarded a redundant execution before it started a run",
    );
    pub(crate) const STALE_REVISION_RETIRED: AbandonCause<'static> = AbandonCause::engine(
        "stale_revision_retired",
        "retire_stale_revision_before_dispatch",
        "conflict already resolved before dispatch; revision advanced to in_review",
    );
    pub(crate) const STALE_GOVERNING_REDISPATCH: AbandonCause<'static> = AbandonCause::engine(
        "stale_governing_redispatch",
        "request_execution_stale_redispatch",
        "governing execution had no live worker; abandoned and replaced with a fresh ready execution",
    );
    pub(crate) const PARENT_CLOSED_REVISION: AbandonCause<'static> = AbandonCause::engine(
        "parent_closed_revision",
        "reconcile_revision_parent_close",
        "parent PR closed; revision's not-yet-live executions withdrawn before it was archived",
    );
    pub(crate) const PARENT_CLOSE_SWEEP: AbandonCause<'static> = AbandonCause::engine(
        "parent_close_sweep",
        "block_pending_revisions_on_parent_close",
        "parent PR closed; pending revision's not-yet-live executions withdrawn before it was archived",
    );
    pub(crate) const RETIRED_SPAWNING_ATTEMPT: AbandonCause<'static> = AbandonCause::engine(
        "retired_spawning_attempt",
        "reconcile_revision_retired_attempt",
        "revision's spawning attempt was retired; its pending item-work executions withdrawn",
    );
    pub(crate) const DEFERRED: AbandonCause<'static> = AbandonCause::engine(
        "deferred",
        "update_task_deferred",
        "item was deferred; not-yet-live executions withdrawn so they cannot dispatch",
    );
    pub(crate) const REVIEW_RESERVATION_RELEASED: AbandonCause<'static> = AbandonCause::engine(
        "review_reservation_released",
        "release_unstartable_pre_merge_review_reservations",
        "pre-merge review batch could not start; member reservations released",
    );
    pub(crate) const REVIEW_BATCH_REAPED: AbandonCause<'static> = AbandonCause::engine(
        "review_batch_reaped",
        "reap_inert_review_batches",
        "review batch's cycle root is gone; member executions retired with the batch",
    );
    pub(crate) const RUN_FINISHED_ABANDONED: AbandonCause<'static> = AbandonCause::engine(
        "run_finished_abandoned",
        "finish_execution_run",
        "run finished and the caller marked the execution abandoned",
    );
}

/// The task-field changes `boss task show` reports from the ledger: the two
/// flags that hold an item out of automatic dispatch. Each has its own
/// trigger, so every write path leaves a row whether or not it went through
/// [`execute`]; a bare write records `unattributed_write`.
pub(super) const FLAG_PROPERTIES: [&str; 2] = ["deferred", "autostart"];

/// Join the concrete write helper (`path`) with the socket caller identity
/// (`provenance`, from `resolve_write_basis`). Engine-internal writers pass
/// `None` and keep the helper name alone.
pub(crate) fn compose_basis(path: &str, provenance: Option<&str>) -> String {
    match provenance.map(str::trim).filter(|value| !value.is_empty()) {
        Some(provenance) => format!("{path} {provenance}"),
        None => path.to_owned(),
    }
}

/// Actor plus optional socket caller identity for a ledger write.
#[derive(Clone, Copy)]
pub(crate) struct WriteAttribution<'a> {
    pub actor: &'a str,
    pub provenance: Option<&'a str>,
}

impl WriteAttribution<'_> {
    pub(crate) fn basis(&self, path: &str) -> String {
        compose_basis(path, self.provenance)
    }
}

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
    let operation = match begin_operation_if_absent(conn) {
        Ok(operation) => operation,
        Err(err) => {
            conn.execute_batch("ROLLBACK TO task_status_write; RELEASE task_status_write")?;
            return Err(err);
        }
    };
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
        operation.forget();
        conn.execute_batch("ROLLBACK TO task_status_write")?;
    } else if let Err(err) = operation.end() {
        conn.execute_batch("ROLLBACK TO task_status_write")?;
        conn.execute_batch("RELEASE task_status_write")?;
        return Err(err);
    }
    conn.execute_batch("RELEASE task_status_write")?;
    result
}

/// One atomic operation across several ledger writes. Every ledger row a
/// write inside it produces carries the same `operation_id`, so a combined
/// change (abandon the pending executions, clear autostart, demote the status)
/// is distinguishable from unrelated writes that share a timestamp.
///
/// [`execute`] and [`abandon`] open an operation of their own when none is
/// active; hold an [`Operation`] to widen it over several writes. It must be
/// begun and ended inside one transaction: the operation row is
/// transaction-local state, and an error return drops the transaction, which
/// rolls the row back with the rest of the writes.
///
/// Drop ends an operation that the caller did not end, so a forgotten `end`
/// cannot leave the singleton in place for later, unrelated writes to join.
/// Call [`Operation::end`] before committing so a delete error is not lost
/// on the drop path.
#[must_use = "end the operation before committing"]
pub(crate) struct Operation<'a> {
    conn: &'a Connection,
    created: bool,
    ended: std::cell::Cell<bool>,
}

impl Operation<'_> {
    pub(crate) fn end(self) -> rusqlite::Result<()> {
        self.end_inner()
    }

    /// The surrounding savepoint is about to roll this row back; Drop must
    /// not DELETE it first, or the rollback would restore the singleton.
    fn forget(&self) {
        self.ended.set(true);
    }

    fn end_inner(&self) -> rusqlite::Result<()> {
        if self.created && !self.ended.get() {
            self.conn.execute("DELETE FROM task_status_audit_operation", [])?;
            self.ended.set(true);
        }
        Ok(())
    }
}

impl Drop for Operation<'_> {
    fn drop(&mut self) {
        if self.created
            && !self.ended.get()
            && let Err(err) = self.end_inner()
        {
            tracing::error!(
                error = %err,
                "task_status_audit operation dropped without end(); later ledger writes may join this operation_id"
            );
        }
    }
}

/// Start an [`Operation`] that spans the writes made before it ends. Joins an
/// operation that is already active.
pub(crate) fn begin_operation<'a>(tx: &'a rusqlite::Transaction<'_>) -> rusqlite::Result<Operation<'a>> {
    begin_operation_on(tx)
}

/// Start an [`Operation`] on a connection that is already inside a
/// transaction. The operation row is transaction-local state; this must not
/// be called outside a transaction, because a committed singleton would
/// correlate every later write until something else ended it.
pub(crate) fn begin_operation_on(conn: &Connection) -> rusqlite::Result<Operation<'_>> {
    begin_operation_if_absent(conn)
}

fn begin_operation_if_absent(conn: &Connection) -> rusqlite::Result<Operation<'_>> {
    match conn.execute(
        "INSERT OR IGNORE INTO task_status_audit_operation (singleton, operation_id) VALUES (1, ?1)",
        params![super::audit_misc::next_id("auditop")],
    ) {
        Ok(created) => Ok(Operation {
            conn,
            created: created > 0,
            ended: std::cell::Cell::new(false),
        }),
        // A schema older than the operation migration records no correlation.
        Err(err) if super::parking::sqlite_missing_table(&err, "task_status_audit_operation") => Ok(Operation {
            conn,
            created: false,
            ended: std::cell::Cell::new(true),
        }),
        Err(err) => Err(err),
    }
}

/// Run one `UPDATE work_executions` that may move rows to `abandoned`, in the
/// writer's own transaction (a savepoint makes an autocommit caller atomic).
/// The trigger on `work_executions` records one `execution_abandoned` ledger
/// row per row it abandons, with `cause`, the previous status, the execution's
/// error text, and whether the execution and its item ever produced a run.
pub(crate) fn abandon<P: rusqlite::Params>(
    conn: &Connection,
    cause: AbandonCause<'_>,
    sql: &str,
    params: P,
) -> rusqlite::Result<usize> {
    conn.execute_batch("SAVEPOINT task_status_abandon")?;
    let operation = match begin_operation_if_absent(conn) {
        Ok(operation) => operation,
        Err(err) => {
            conn.execute_batch("ROLLBACK TO task_status_abandon; RELEASE task_status_abandon")?;
            return Err(err);
        }
    };
    let result = (|| {
        conn.execute(
            "INSERT INTO task_status_audit_context (basis, actor, reason_code, detail) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![cause.path, cause.actor, cause.code, cause.detail],
        )?;
        let count = conn.execute(sql, params)?;
        conn.execute("DELETE FROM task_status_audit_context", [])?;
        Ok(count)
    })();
    if result.is_err() {
        operation.forget();
        conn.execute_batch("ROLLBACK TO task_status_abandon")?;
    } else if let Err(err) = operation.end() {
        conn.execute_batch("ROLLBACK TO task_status_abandon")?;
        conn.execute_batch("RELEASE task_status_abandon")?;
        return Err(err);
    }
    conn.execute_batch("RELEASE task_status_abandon")?;
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

/// Record PR observations so progression can age the condition itself rather
/// than the last unrelated task edit. Existing observations have unknown age.
pub(super) fn migrate_pr_observations(conn: &Connection) -> Result<()> {
    for property in ["pr_mergeable_state", "ci_required_state"] {
        conn.execute_batch(&format!(
            "INSERT INTO task_status_audit
                (work_item_id, property, new_value, actor, changed_at, basis, age_is_lower_bound)
             SELECT id, '{property}', {property}, 'engine', strftime('%s','now'), 'unknown_legacy', 1
             FROM tasks;"
        ))?;
        install_property_triggers(conn, property)?;
    }
    Ok(())
}

/// Columns this module adds to the ledger for enriched history.
const ENRICHED_COLUMNS: [(&str, &str); 8] = [
    ("operation_id", "TEXT"),
    ("reason_code", "TEXT"),
    ("detail", "TEXT"),
    ("execution_id", "TEXT"),
    ("failure_stamp", "TEXT"),
    ("failure_error", "TEXT"),
    ("execution_ever_ran", "INTEGER"),
    ("item_ever_ran", "INTEGER"),
];

/// Make the ledger carry abandonment history and operation correlation, then
/// reinstall the recorders with the enriched shape. Rows recorded before this
/// migration keep NULL in every added column: nothing is invented for a
/// transition whose cause was never captured. Idempotent.
pub(super) fn migrate_execution_abandon(conn: &Connection) -> Result<()> {
    for (column, ty) in ENRICHED_COLUMNS {
        if !crate::schema_init::table_has_column(conn, "task_status_audit", column)? {
            conn.execute_batch(&format!("ALTER TABLE task_status_audit ADD COLUMN {column} {ty}"))?;
        }
    }
    for column in ["reason_code", "detail"] {
        if !crate::schema_init::table_has_column(conn, "task_status_audit_context", column)? {
            conn.execute_batch(&format!(
                "ALTER TABLE task_status_audit_context ADD COLUMN {column} TEXT"
            ))?;
        }
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_status_audit_operation (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1) DEFAULT 1,
            operation_id TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS task_status_audit_operation_id
            ON task_status_audit(operation_id) WHERE operation_id IS NOT NULL;",
    )?;
    for property in [
        "status",
        "blocked_reason",
        "dispatch_failed_reason",
        "autostart",
        "deferred",
    ] {
        conn.execute_batch(&format!(
            "DROP TRIGGER IF EXISTS task_status_audit_insert_{property};
             DROP TRIGGER IF EXISTS task_status_audit_update_{property};"
        ))?;
        install_property_triggers(conn, property)?;
    }
    install_abandon_trigger(conn)
}

/// Record every `work_executions` transition to `abandoned`, whichever writer
/// made it. The writer's cause comes from the context [`abandon`] set; a write
/// that did not set one records `unattributed_write`.
fn install_abandon_trigger(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "DROP TRIGGER IF EXISTS task_status_audit_abandon_execution;
         CREATE TRIGGER task_status_audit_abandon_execution
         AFTER UPDATE OF status ON work_executions
         WHEN NEW.status = 'abandoned' AND OLD.status IS NOT 'abandoned'
         BEGIN
            INSERT INTO task_status_audit
                (work_item_id, property, old_value, new_value, actor, changed_at, basis,
                 operation_id, reason_code, detail, execution_id, failure_stamp, failure_error,
                 execution_ever_ran, item_ever_ran)
            VALUES (NEW.work_item_id, '{EXECUTION_ABANDONED_PROPERTY}', OLD.status, 'abandoned',
                COALESCE((SELECT actor FROM task_status_audit_context), 'engine'),
                strftime('%s','now'),
                COALESCE((SELECT basis FROM task_status_audit_context), '{UNATTRIBUTED_WRITE}'),
                (SELECT operation_id FROM task_status_audit_operation),
                COALESCE((SELECT reason_code FROM task_status_audit_context), '{UNATTRIBUTED_WRITE}'),
                (SELECT detail FROM task_status_audit_context),
                NEW.id,
                (SELECT dispatch_failed_at FROM tasks WHERE id = NEW.work_item_id),
                COALESCE(NEW.last_error, (SELECT dispatch_failed_error FROM tasks WHERE id = NEW.work_item_id)),
                {EXECUTION_EVER_RAN_SQL},
                {ITEM_EVER_RAN_SQL});
         END;"
    ))?;
    Ok(())
}

/// SQL boolean: this execution had started or produced a run.
const EXECUTION_EVER_RAN_SQL: &str =
    "NEW.started_at IS NOT NULL OR EXISTS (SELECT 1 FROM work_runs WHERE execution_id = NEW.id)";

/// SQL boolean: any execution of the item had started or produced a run.
/// Uses the same "started or produced a run" definition as [`EXECUTION_EVER_RAN_SQL`].
const ITEM_EVER_RAN_SQL: &str = "EXISTS (
    SELECT 1 FROM work_executions we
    WHERE we.work_item_id = NEW.work_item_id
      AND (we.started_at IS NOT NULL
           OR EXISTS (SELECT 1 FROM work_runs wr WHERE wr.execution_id = we.id)))";

/// [`ITEM_EVER_RAN_SQL`] against a `tasks` trigger's `NEW.id`.
const ITEM_EVER_RAN_ON_TASK_SQL: &str = "EXISTS (
    SELECT 1 FROM work_executions we
    WHERE we.work_item_id = NEW.id
      AND (we.started_at IS NOT NULL
           OR EXISTS (SELECT 1 FROM work_runs wr WHERE wr.execution_id = we.id)))";

impl WorkDb {
    /// The recorded history of one task, oldest first: its `deferred` /
    /// `autostart` changes, every execution abandonment, and the status moves
    /// that happened in the same operation as one of those.
    pub fn list_task_field_audit(&self, work_item_id: &str) -> Result<Vec<boss_protocol::TaskFieldAuditRow>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT property, old_value, new_value, actor, basis, changed_at, age_is_lower_bound,
                    reason_code, detail, operation_id, execution_id, failure_stamp, failure_error,
                    execution_ever_ran, item_ever_ran
             FROM task_status_audit a
             WHERE work_item_id = ?1
               AND (property IN (?2, ?3, ?4)
                    OR (property = 'status' AND operation_id IS NOT NULL AND operation_id IN (
                        SELECT operation_id FROM task_status_audit
                        WHERE work_item_id = ?1 AND property IN (?2, ?3, ?4))))
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map(
                params![
                    work_item_id,
                    FLAG_PROPERTIES[0],
                    FLAG_PROPERTIES[1],
                    EXECUTION_ABANDONED_PROPERTY
                ],
                |row| {
                    Ok(boss_protocol::TaskFieldAuditRow {
                        property: row.get(0)?,
                        old_value: row.get(1)?,
                        new_value: row.get(2)?,
                        actor: row.get(3)?,
                        basis: row.get(4)?,
                        changed_at: row.get(5)?,
                        age_is_lower_bound: row.get(6)?,
                        reason_code: row.get(7)?,
                        detail: row.get(8)?,
                        operation_id: row.get(9)?,
                        execution_id: row.get(10)?,
                        failure_stamp: row.get(11)?,
                        failure_error: row.get(12)?,
                        execution_ever_ran: row.get(13)?,
                        item_ever_ran: row.get(14)?,
                    })
                },
            )?
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
    let enriched = crate::schema_init::table_has_column(conn, "task_status_audit", "operation_id")?;
    // The extra columns every recorder fills once the ledger has them. The
    // reason code defaults to the write helper's name, the first word of the
    // basis (the rest is the socket caller's identity).
    let (columns, values) = if enriched {
        let ran = if matches!(property, "status" | "autostart" | "deferred") {
            ITEM_EVER_RAN_ON_TASK_SQL
        } else {
            "NULL"
        };
        let failure = if matches!(property, "status" | "autostart" | "deferred") {
            "NEW.dispatch_failed_at, NEW.dispatch_failed_error"
        } else {
            "NULL, NULL"
        };
        (
            ", operation_id, reason_code, failure_stamp, failure_error, item_ever_ran".to_owned(),
            format!(
                ", (SELECT operation_id FROM task_status_audit_operation),
                COALESCE((SELECT reason_code FROM task_status_audit_context),
                    (SELECT substr(b, 1, instr(b || ' ', ' ') - 1)
                     FROM (SELECT COALESCE((SELECT basis FROM task_status_audit_context), '{UNATTRIBUTED_WRITE}') AS b))),
                {failure}, {ran}"
            ),
        )
    } else {
        (String::new(), String::new())
    };
    conn.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS task_status_audit_update_{property}
         AFTER UPDATE OF {property} ON tasks
         WHEN OLD.{property} IS NOT NEW.{property} {repeated}
         BEGIN
            INSERT INTO task_status_audit
                (work_item_id, property, old_value, new_value, actor, changed_at, basis{columns})
            VALUES (NEW.id, '{property}', OLD.{property}, NEW.{property},
                COALESCE((SELECT actor FROM task_status_audit_context), NEW.last_status_actor),
                strftime('%s','now'),
                COALESCE((SELECT basis FROM task_status_audit_context), '{UNATTRIBUTED_WRITE}'){values});
         END;
         CREATE TRIGGER IF NOT EXISTS task_status_audit_insert_{property}
         AFTER INSERT ON tasks BEGIN
            INSERT INTO task_status_audit
                (work_item_id, property, old_value, new_value, actor, changed_at, basis{columns})
            VALUES (NEW.id, '{property}', NULL, NEW.{property},
                COALESCE((SELECT actor FROM task_status_audit_context), NEW.last_status_actor),
                strftime('%s','now'),
                COALESCE((SELECT basis FROM task_status_audit_context), '{UNATTRIBUTED_WRITE}'){values});
         END;"
    ))?;
    Ok(())
}
