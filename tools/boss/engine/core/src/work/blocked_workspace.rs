//! Durable workspace handoff after a worker explicitly declares itself blocked.
use super::*;

/// Only the immediate predecessor of this item is eligible. A terminal status
/// alone, an input preference, or an older blocked run is not recovery evidence.
pub(super) fn blocked_workspace_predecessor(
    conn: &Connection,
    work_item_id: &str,
    exclude_execution_id: &str,
) -> Result<Option<WorkExecution>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM work_executions WHERE work_item_id = ?1 AND id != ?2
         ORDER BY created_at DESC, id DESC LIMIT 1",
            params![work_item_id, exclude_execution_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = id else { return Ok(None) };
    let blocked: bool = conn.query_row(
        "SELECT COALESCE(run_done_outcome = 'blocked', 0) FROM work_executions WHERE id = ?1",
        [&id],
        |row| row.get(0),
    )?;
    Ok(query_execution(conn, &id)?.filter(|e| {
        blocked && e.status.is_terminal() && e.preferred_workspace_id.as_deref().is_some_and(|id| !id.is_empty())
    }))
}

impl WorkDb {
    pub(crate) fn blocked_workspace_predecessor(&self, execution: &WorkExecution) -> Result<Option<WorkExecution>> {
        if !execution.allow_dirty || !execution.prefer_is_soft {
            return Ok(None);
        }
        let conn = self.connect()?;
        Ok(
            blocked_workspace_predecessor(&conn, &execution.work_item_id, &execution.id)?
                .filter(|prior| prior.preferred_workspace_id == execution.preferred_workspace_id),
        )
    }
}
