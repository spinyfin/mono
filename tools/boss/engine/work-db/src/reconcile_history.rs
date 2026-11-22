//! Reconciliation diagnostics belong to the durable audit ledger, never the brief.
use super::*;

pub(super) const MIGRATION_ID: &str = "20261122_0423_reconcile_history";

#[work_db_boundary::instrument]
impl WorkDb {
    /// Record the complete reconciliation narrative for operator inspection via
    /// `list_task_field_audit`. This does not mutate the item or its timestamps.
    pub fn record_reconcile_audit(&self, work_item_id: &str, epoch: i64, detail: &str) -> Result<()> {
        self.get_work_item(work_item_id)?;
        let conn = self.connect()?;
        insert(&conn, work_item_id, epoch, detail, "reconciler")
    }
}

fn insert(conn: &Connection, id: &str, epoch: i64, detail: &str, basis: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO task_status_audit
            (work_item_id, property, actor, changed_at, basis, detail)
         VALUES (?1, 'reconciliation', 'engine', ?2, ?3, ?4)",
        params![id, epoch.to_string(), basis, detail],
    )?;
    Ok(())
}

/// Move legacy appended lines in the same startup transaction that removes
/// them from descriptions. Keep duplicates, full text (including old patch
/// paths), and original epochs; never infer an execution association.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    for table in ["products", "projects", "tasks"] {
        let mut stmt = conn.prepare(&format!(
            "SELECT id, description FROM {table}
             WHERE description LIKE '%[engine-reconcile] epoch %'
                OR description LIKE '%[parking] epoch %'"
        ))?;
        let items = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, description) in items {
            let mut brief = String::new();
            let mut moved = false;
            let mut fence: Option<(char, usize)> = None;
            // Appenders owned the newline BEFORE their record. Preserve all
            // other bytes, including authored trailing newlines and CRLF.
            for (index, line) in description.split('\n').enumerate() {
                let trimmed = line.trim_start();
                if let Some((marker, width)) = fence {
                    if trimmed.chars().take_while(|c| *c == marker).count() >= width
                        && trimmed.trim_start_matches(marker).trim().is_empty()
                    {
                        fence = None;
                    }
                } else if trimmed.starts_with("```") {
                    fence = Some(('`', trimmed.chars().take_while(|c| *c == '`').count()));
                } else if trimmed.starts_with("~~~") {
                    fence = Some(('~', trimmed.chars().take_while(|c| *c == '~').count()));
                } else if let Some(epoch) = legacy_epoch(line) {
                    insert(conn, &id, epoch, line, "legacy_description")?;
                    moved = true;
                    continue;
                }
                if index > 0 {
                    brief.push('\n');
                }
                brief.push_str(line);
            }
            if moved {
                conn.execute(
                    &format!("UPDATE {table} SET description = ?2 WHERE id = ?1"),
                    params![id, brief],
                )?;
            }
        }
    }
    Ok(())
}

fn legacy_epoch(line: &str) -> Option<i64> {
    let rest = line
        .strip_prefix("[engine-reconcile] epoch ")
        .or_else(|| line.strip_prefix("[parking] epoch "))?;
    let (epoch, reason) = rest.split_once(": ")?;
    if reason.is_empty() || !reason.ends_with('.') {
        return None;
    }
    epoch.parse().ok()
}
