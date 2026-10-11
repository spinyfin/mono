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

fn has_execution_in(conn: &Connection, task_id: &str, statuses: &str) -> Result<bool> {
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM work_executions
             WHERE work_item_id = ?1 AND kind = 'revision_implementation' AND status IN ({statuses}))"
        ),
        [task_id],
        |row| row.get(0),
    )?)
}

/// A writer execution waiting for dispatch, regardless of the task's status
/// (a restarted `in_review` revision keeps its status until it starts).
fn has_pending_writer(conn: &Connection, task_id: &str) -> Result<bool> {
    has_execution_in(conn, task_id, "'queued', 'ready', 'waiting_dependency'")
}

fn is_conflict_revision(conn: &Connection, task_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND kind = 'revision'
         AND created_via LIKE ?2)",
        params![task_id, format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}%")],
        |row| row.get(0),
    )?)
}

/// A conflict fix still in flight: queued or running, or waiting on its own
/// prerequisites. Judged by task state rather than executions because the fix
/// has no execution yet when it is minted. A failed or parked fix is not in
/// flight and must not strand work behind it.
fn conflict_fix_in_flight(conn: &Connection, task_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND deleted_at IS NULL
         AND (status IN ('todo', 'active')
              OR (status = 'blocked' AND blocked_reason = 'dependency')))",
        [task_id],
        |row| row.get(0),
    )?)
}

/// Same-chain prerequisites of a conflict fix represent writer exclusion.
/// Explicit prerequisites outside this PR retain ordinary completion semantics.
pub(crate) fn conflict_writer_gate(
    conn: &Connection,
    dependent_id: &str,
    prerequisite_id: &str,
) -> Result<Option<bool>> {
    if !is_conflict_revision(conn, dependent_id)?
        || chain_root(conn, dependent_id)? != chain_root(conn, prerequisite_id)?
    {
        return Ok(None);
    }
    // A queued older conflict fix may itself have been moved behind a newer
    // conflict fix. It waits only while the newer fix is in flight, so a
    // failed newer fix does not strand it. Task IDs encode creation time, as
    // in the revision tail ordering.
    if prerequisite_id > dependent_id {
        return Ok(Some(conflict_fix_in_flight(conn, prerequisite_id)?));
    }
    Ok(Some(has_writer(conn, prerequisite_id)?))
}

/// Outstanding newer conflict fixes that `work_item_id` must queue behind.
///
/// Mint-time reordering is a snapshot; a writer that later returns to the
/// queue (a released dispatch claim, a retry after failure) or a restarted
/// `in_review` revision would otherwise run ahead of the fix. A revision with
/// a live writer is never gated, so this cannot cycle with the writer
/// exclusion edge from the conflict fix to that writer.
pub(crate) fn queued_behind_conflict_fixes(conn: &Connection, work_item_id: &str) -> Result<Vec<String>> {
    let Some(task) = query_task(conn, work_item_id)? else {
        return Ok(Vec::new());
    };
    if task.kind != TaskKind::Revision
        || !(task.status.can_still_change_pr() || has_pending_writer(conn, work_item_id)?)
        || has_writer(conn, work_item_id)?
    {
        return Ok(Vec::new());
    }
    let root_id = chain_root(conn, work_item_id)?;
    let mut members = collect_chain_revision_ids_including_deleted(conn, &root_id)?;
    members.sort();
    let mut gates = Vec::new();
    for id in members {
        if id.as_str() > work_item_id
            && is_conflict_revision(conn, &id)?
            && query_task(conn, &id)?.is_some()
            && conflict_fix_in_flight(conn, &id)?
        {
            gates.push(id);
        }
    }
    Ok(gates)
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
        // A restarted `in_review` revision is still unsettled while it holds
        // a pending writer execution, even though its status has not moved.
        let can_change = task.status.can_still_change_pr();
        if task.kind != TaskKind::Revision || !(can_change || has_pending_writer(conn, id)?) {
            continue;
        }
        // Keep the existing queued order and external prerequisites. Every
        // queued row also waits for the conflict fix, including a failed
        // row that a human may retry while the fix is in flight.
        deps::delete_edge(conn, revision_id, id, RELATION_BLOCKS)?;
        deps::insert_edge(conn, id, revision_id, RELATION_BLOCKS, now)?;
        // Read-only reviews may still be attached to this row. They do not
        // own the branch and must not be cancelled by the task-status helper.
        if can_change && query_live_execution_for_work_item(conn, id)?.is_none() {
            maybe_engine_block_dependent(pending, conn, id, now)?;
        }
        // Demote existing ready rows atomically; never mint a replacement
        // execution for a failed/parked revision merely because we reordered it.
        let mut stmt = conn.prepare(
            "SELECT id FROM work_executions
             WHERE work_item_id = ?1 AND kind = 'revision_implementation'
               AND status IN ('queued', 'ready')",
        )?;
        let ids = stmt
            .query_map([id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for execution_id in ids {
            if let Some(demoted) = update_execution_status(conn, &execution_id, ExecutionStatus::WaitingDependency)? {
                stage_execution_status_changed(pending, &demoted);
            }
        }
    }
    maybe_engine_block_dependent(pending, conn, revision_id, now)?;
    Ok(())
}

/// Promote `waiting_dependency` writer executions whose conflict-fix gate has
/// cleared back to `ready`. `maybe_engine_unblock_dependent` only wakes
/// `blocked` tasks and the stuck-execution sweep only looks at `todo` ones, but
/// a restarted `in_review` revision or a released/retried `active` writer is
/// parked behind a conflict fix while keeping its task status, so it needs its
/// own wake-up. `task_ids` limits the scan; `None` scans every task.
pub(crate) fn promote_waiting_writers(
    pending: &mut PendingEvents,
    conn: &Connection,
    task_ids: Option<&[String]>,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT we.id, we.work_item_id FROM work_executions we
         JOIN tasks t ON t.id = we.work_item_id
         WHERE we.status = 'waiting_dependency' AND we.kind = 'revision_implementation'
           AND t.kind = 'revision' AND t.deleted_at IS NULL
           AND t.status IN ('active', 'in_review')
         ORDER BY we.created_at ASC, we.id ASC",
    )?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let mut promoted = Vec::new();
    for (execution_id, work_item_id) in rows {
        if task_ids.is_some_and(|ids| !ids.contains(&work_item_id))
            || query_live_execution_for_work_item(conn, &work_item_id)?.is_some()
            || !deps::gating_prereqs_for(conn, &work_item_id)?.is_empty()
        {
            continue;
        }
        if let Some(ready) = update_execution_status(conn, &execution_id, ExecutionStatus::Ready)? {
            stage_execution_status_changed(pending, &ready);
            pending.push(Event::DispatchReady);
            promoted.push(work_item_id);
        }
    }
    Ok(promoted)
}

/// Cascade hook: when a conflict fix changes state, wake the same-chain
/// writers that were parked behind it.
pub(crate) fn wake_writers_after_conflict_change(
    pending: &mut PendingEvents,
    conn: &Connection,
    task_id: &str,
) -> Result<()> {
    if !is_conflict_revision(conn, task_id)? {
        return Ok(());
    }
    let root_id = chain_root(conn, task_id)?;
    let mut members = collect_chain_revision_ids_including_deleted(conn, &root_id)?;
    members.push(root_id);
    promote_waiting_writers(pending, conn, Some(&members))?;
    Ok(())
}
