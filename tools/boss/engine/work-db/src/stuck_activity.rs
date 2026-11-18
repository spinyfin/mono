//! Shared-PR executions count as progress even when their parent is blocked.
use super::*;

pub(super) fn has_pending_execution(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_executions WHERE work_item_id = ?1
        AND status IN ('queued','ready'))",
        [id],
        |r| r.get(0),
    )?)
}

pub(super) fn has_waiting_dependency_execution(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_executions WHERE work_item_id = ?1
        AND status = 'waiting_dependency')",
        [id],
        |r| r.get(0),
    )?)
}

pub(super) fn related_work_is_live(conn: &Connection, task: &Task) -> Result<bool> {
    let root = get_chain_root_task(conn, &task.id)?.unwrap_or_else(|| task.clone());
    // Follow revision parents just as chain-root PR projection does. UNION also
    // bounds malformed cycles. Direct PR bindings include unrelated chain roots.
    // `waiting_dependency` is a gate, not evidence anyone is working the PR.
    Ok(conn.query_row(
        "WITH RECURSIVE family(id) AS (
            SELECT id FROM tasks WHERE deleted_at IS NULL
                AND (id IN (?1, ?3) OR pr_url = NULLIF(?2, '') OR pr_url = NULLIF(?4, ''))
            UNION
            SELECT t.id FROM tasks t JOIN family f ON t.parent_task_id = f.id
            WHERE t.kind = 'revision' AND t.deleted_at IS NULL
        ) SELECT EXISTS(SELECT 1 FROM family f JOIN work_executions e ON e.work_item_id = f.id
            WHERE f.id != ?3 AND e.status IN ('queued','ready','claimed','running','waiting_human'))",
        params![root.id, root.pr_url, task.id, task.pr_url],
        |r| r.get(0),
    )?)
}

pub(super) fn review_is_stalled(conn: &Connection, task: &Task) -> Result<bool> {
    let unproductive = super::review_verdicts::unproductive_completed_pr_review_sql();
    Ok(conn
        .query_row(
            &format!(
                "SELECT we.status IN ('failed','cancelled','abandoned','orphaned') OR {unproductive}
            FROM work_executions we WHERE we.work_item_id = ?1 AND we.kind = 'pr_review'
            ORDER BY we.created_at DESC, we.id DESC LIMIT 1"
            ),
            [&task.id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(task.review_required_state.as_deref() != Some("approved")))
}
