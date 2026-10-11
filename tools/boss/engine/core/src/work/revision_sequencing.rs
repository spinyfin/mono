//! Conflict revisions jump ahead of queued work within the create transaction.
use super::*;

fn has_writer(conn: &Connection, task_id: &str) -> Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT kind FROM work_executions
         WHERE work_item_id = ?1 AND status IN ('claimed', 'running', 'waiting_human')",
    )?;
    let kinds = stmt.query_map([task_id], |row| row.get::<_, String>(0))?;
    for kind in kinds {
        let kind: ExecutionKind = kind?.parse().map_err(anyhow::Error::msg)?;
        if !kind.is_read_only_review() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Same-chain prerequisites of a conflict fix represent writer exclusion.
/// Explicit prerequisites outside this PR retain ordinary completion semantics.
pub(crate) fn conflict_writer_gate(
    conn: &Connection,
    dependent_id: &str,
    prerequisite_id: &str,
) -> Result<Option<bool>> {
    let is_conflict: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND kind = 'revision'
         AND created_via LIKE ?2)",
        params![dependent_id, format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}%")],
        |row| row.get(0),
    )?;
    // A queued older conflict fix may itself have been moved behind a newer
    // conflict fix. That forward edge must retain normal completion gating.
    // Task IDs encode creation time, as in the revision tail ordering.
    if !is_conflict
        || prerequisite_id > dependent_id
        || chain_root(conn, dependent_id)? != chain_root(conn, prerequisite_id)?
    {
        return Ok(None);
    }
    Ok(Some(has_writer(conn, prerequisite_id)?))
}

pub(super) fn prioritize_conflict_revision(
    pending: &mut PendingEvents,
    conn: &Connection,
    revision_id: &str,
    root_id: &str,
    now: &str,
) -> Result<()> {
    let mut members = collect_chain_revision_ids_including_deleted(conn, root_id)?;
    members.push(root_id.to_owned());
    for id in &members {
        if id == revision_id {
            continue;
        }
        if has_writer(conn, id)? {
            deps::insert_edge(conn, revision_id, id, RELATION_BLOCKS, now)?;
            continue;
        }
        let Some(task) = query_task(conn, id)? else { continue };
        if task.kind != TaskKind::Revision || !task.status.can_still_change_pr() {
            continue;
        }
        // Keep the existing queued order and external prerequisites. Every
        // queued row also waits for the conflict fix, including a failed
        // row that a human may retry while the fix is in flight.
        deps::delete_edge(conn, revision_id, id, RELATION_BLOCKS)?;
        deps::insert_edge(conn, id, revision_id, RELATION_BLOCKS, now)?;
        // Read-only reviews may still be attached to this row. They do not
        // own the branch and must not be cancelled by the task-status helper.
        if query_live_execution_for_work_item(conn, id)?.is_none() {
            maybe_engine_block_dependent(pending, conn, id, now)?;
        }
        // Demote existing ready rows atomically; never mint a replacement
        // execution for a failed/parked revision merely because we reordered it.
        conn.execute(
            "UPDATE work_executions SET status = 'waiting_dependency'
             WHERE work_item_id = ?1 AND kind = 'revision_implementation'
               AND status IN ('queued', 'ready')",
            [id],
        )?;
    }
    maybe_engine_block_dependent(pending, conn, revision_id, now)?;
    Ok(())
}
