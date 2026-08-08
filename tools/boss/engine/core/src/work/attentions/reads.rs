//! Attention projections shared by standalone reads and the work-tree snapshot.
use super::*;

pub(super) fn list_groups(
    conn: &Connection,
    product_id: &str,
    project_id: Option<&str>,
    task_id: Option<&str>,
    kind: Option<&str>,
    state: Option<&str>,
) -> Result<Vec<AttentionGroup>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {GROUP_COLS} FROM attention_groups
             WHERE product_id = ?1
               AND (?2 IS NULL OR association_project_id = ?2)
               AND (?3 IS NULL OR association_task_id = ?3)
               AND (?4 IS NULL OR kind = ?4)
               AND (
                    (?5 IS NULL AND state IN ('open', 'partially_answered'))
                    OR state = ?5
               )
             ORDER BY created_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map(
        params![product_id, project_id, task_id, kind, state],
        map_attention_group,
    )?;
    collect_rows(rows)
}

pub(in crate::work) fn list_groups_with_members(
    conn: &Connection,
    product_id: &str,
    project_id: Option<&str>,
    task_id: Option<&str>,
    kind: Option<&str>,
    state: Option<&str>,
) -> Result<(Vec<AttentionGroup>, Vec<Attention>)> {
    let groups = list_groups(conn, product_id, project_id, task_id, kind, state)?;
    if groups.is_empty() {
        return Ok((groups, Vec::new()));
    }
    // A JSON array binds any number of group IDs without SQLite's parameter
    // limit. One member query, regardless of the number of groups.
    let ids = serde_json::to_string(&groups.iter().map(|group| &group.id).collect::<Vec<_>>())?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {ATTN_COLS} FROM attentions
         WHERE group_id IN (SELECT value FROM json_each(?1))
         ORDER BY group_id, ordinal ASC, id ASC"
    ))?;
    let members = collect_rows(stmt.query_map([ids], map_attention)?)?;
    Ok((groups, members))
}

impl WorkDb {
    /// Groups and members from a single read snapshot, in two SELECTs.
    pub fn list_attention_groups_with_members(
        &self,
        product_id: &str,
        project_id: Option<&str>,
        task_id: Option<&str>,
        kind: Option<&str>,
        state: Option<&str>,
    ) -> Result<(Vec<AttentionGroup>, Vec<Attention>)> {
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        list_groups_with_members(&tx, product_id, project_id, task_id, kind, state)
    }
}

pub(in crate::work) fn list_items(conn: &Connection, work_item_id: &str) -> Result<Vec<WorkAttentionItem>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.execution_id, a.work_item_id, a.kind, a.status, a.title, a.body_markdown,
                a.created_at, a.resolved_at, a.converted_task_id
         FROM work_attention_items a
         WHERE a.work_item_id = ?1
            OR a.execution_id IN (SELECT e.id FROM work_executions e WHERE e.work_item_id = ?1)
         ORDER BY a.created_at ASC, a.id ASC",
    )?;
    let rows = stmt.query_map([work_item_id], map_attention_item)?;
    collect_rows(rows)
}
