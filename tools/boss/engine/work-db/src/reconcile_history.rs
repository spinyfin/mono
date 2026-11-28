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
         VALUES (?1, ?2, 'engine', ?3, ?4, ?5)",
        params![id, RECONCILIATION_PROPERTY, epoch.to_string(), basis, detail],
    )?;
    Ok(())
}

fn fence_after_indent(line: &str) -> Option<&str> {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    (indent <= 3).then(|| &line[indent..])
}

fn open_fence(line: &str) -> Option<(char, usize)> {
    let after = fence_after_indent(line)?;
    if after.starts_with("```") {
        let width = after.bytes().take_while(|&b| b == b'`').count();
        (!after[width..].contains('`')).then_some(('`', width))
    } else if after.starts_with("~~~") {
        let width = after.bytes().take_while(|&b| b == b'~').count();
        Some(('~', width))
    } else {
        None
    }
}

fn is_closing_fence(line: &str, marker: char, width: usize) -> bool {
    let Some(after) = fence_after_indent(line) else {
        return false;
    };
    let count = after.bytes().take_while(|&b| b as char == marker).count();
    count >= width && after[count..].trim().is_empty()
}

fn partition_legacy_records(description: &str) -> (String, Vec<(i64, &str)>) {
    let mut ignored_openers = HashSet::new();
    loop {
        let mut brief = String::new();
        let mut records = Vec::new();
        let mut fence: Option<(usize, char, usize)> = None;
        // Appenders owned the newline BEFORE their record. Preserve all
        // other bytes, including authored trailing newlines and CRLF.
        for (index, line) in description.split('\n').enumerate() {
            if let Some((_, marker, width)) = fence {
                if is_closing_fence(line, marker, width) {
                    fence = None;
                }
            } else if let Some((marker, width)) =
                (!ignored_openers.contains(&index)).then(|| open_fence(line)).flatten()
            {
                fence = Some((index, marker, width));
            } else if let Some(epoch) = legacy_epoch(line) {
                records.push((epoch, line));
                continue;
            }
            if index > 0 {
                brief.push('\n');
            }
            brief.push_str(line);
        }
        match fence {
            Some((opener_index, _, _)) => {
                ignored_openers.insert(opener_index);
            }
            None => return (brief, records),
        }
    }
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
            let (brief, records) = partition_legacy_records(&description);
            if !records.is_empty() {
                for (epoch, line) in records {
                    insert(conn, &id, epoch, line, "legacy_description")?;
                }
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
