//! Upgrade the original attachment schema so evidence retention is independent
//! from execution retention.

use super::*;

/// Columns this rebuild copies. A live column missing from this list would be
/// dropped silently, so [`migrate_work_attachments_execution_retention`]
/// refuses to proceed unless every live column is named here.
const WORK_ATTACHMENTS_RETENTION_COLUMNS: &[&str] = &[
    "id",
    "execution_id",
    "work_item_id",
    "caption",
    "content_digest",
    "media_type",
    "pixel_width",
    "pixel_height",
    "size_bytes",
    "source_name",
    "created_at",
    "reclaimed_at",
];

/// Rebuild legacy `work_attachments` tables that cascade-delete evidence when
/// an execution is pruned. The row has enough denormalized provenance to stay
/// useful without the execution row, and attachment retention then remains the
/// only path that reclaims its bytes and stamps its tombstone.
pub(crate) fn migrate_work_attachments_execution_retention(conn: &Connection) -> Result<()> {
    if !work_attachments_references_executions(conn)? {
        return Ok(());
    }

    // SQLite cannot drop a foreign-key constraint in place. Stage a complete
    // copy before replacing the child table; no foreign-key pragma change is
    // needed because `work_attachments` is the child, never an FK parent.
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let rebuilt = (|| -> Result<()> {
        // Un-wedge a database left holding a committed scratch table from a
        // previous interrupted rebuild (`migrate_projects_tasks_status_check`
        // added the same first-statement DROP for that reason).
        conn.execute_batch("DROP TABLE IF EXISTS work_attachments_v2;")?;
        let live = live_column_names(conn, "work_attachments")?;
        let unknown: Vec<&str> = live
            .iter()
            .map(String::as_str)
            .filter(|name| !WORK_ATTACHMENTS_RETENTION_COLUMNS.contains(name))
            .collect();
        if !unknown.is_empty() {
            bail!(
                "`work_attachments` carries column(s) [{}] that the retention rebuild has no \
                 declaration for; add them to this migration's column list — rebuilding \
                 without them would drop the data",
                unknown.join(", ")
            );
        }
        conn.execute_batch(
            "CREATE TABLE work_attachments_v2 (
                 id             TEXT PRIMARY KEY,
                 execution_id   TEXT NOT NULL,
                 work_item_id   TEXT NOT NULL,
                 caption        TEXT NOT NULL DEFAULT '',
                 content_digest TEXT NOT NULL,
                 media_type     TEXT NOT NULL,
                 pixel_width    INTEGER NOT NULL,
                 pixel_height   INTEGER NOT NULL,
                 size_bytes     INTEGER NOT NULL,
                 source_name    TEXT NOT NULL,
                 created_at     TEXT NOT NULL,
                 reclaimed_at   TEXT,
                 UNIQUE (execution_id, content_digest)
             );
             INSERT INTO work_attachments_v2
                 (id, execution_id, work_item_id, caption, content_digest, media_type,
                  pixel_width, pixel_height, size_bytes, source_name, created_at, reclaimed_at)
             SELECT id, execution_id, work_item_id, caption, content_digest, media_type,
                    pixel_width, pixel_height, size_bytes, source_name, created_at, reclaimed_at
               FROM work_attachments;",
        )?;
        let source_rows: i64 = conn.query_row("SELECT COUNT(*) FROM work_attachments", [], |row| row.get(0))?;
        let staged_rows: i64 = conn.query_row("SELECT COUNT(*) FROM work_attachments_v2", [], |row| row.get(0))?;
        if source_rows != staged_rows {
            bail!(
                "the work_attachments retention migration staged {staged_rows} of {source_rows} row(s); \
                 refusing to replace the original table with an incomplete copy"
            );
        }
        conn.execute_batch(
            "DROP TABLE work_attachments;
             ALTER TABLE work_attachments_v2 RENAME TO work_attachments;
             CREATE INDEX work_attachments_work_item_idx
                 ON work_attachments(work_item_id, created_at);
             CREATE INDEX work_attachments_digest_idx
                 ON work_attachments(content_digest);",
        )?;
        Ok(())
    })();
    match rebuilt {
        Ok(()) => conn.execute_batch("COMMIT;")?,
        Err(err) => {
            if let Err(rollback_err) = conn.execute_batch("ROLLBACK;") {
                tracing::error!(
                    error = %rollback_err,
                    "rolling back failed work_attachments retention migration also failed"
                );
            }
            return Err(err);
        }
    }
    Ok(())
}

/// Whether this is the original attachment schema, whose execution foreign
/// key made execution retention bypass attachment tombstones.
fn work_attachments_references_executions(conn: &Connection) -> Result<bool> {
    conn.prepare(
        "SELECT 1 FROM pragma_foreign_key_list('work_attachments')
         WHERE \"table\" = 'work_executions'",
    )?
    .exists([])
    .map_err(Into::into)
}
