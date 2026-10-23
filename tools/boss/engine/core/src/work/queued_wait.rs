use super::*;
use boss_protocol::DispatchWaitBlocker;

#[derive(Default)]
pub(super) struct DispatchWait {
    pub reason: Option<String>,
    pub not_before: Option<String>,
    pub blocker: Option<DispatchWaitBlocker>,
    /// The projection consulted another work item's state (dependency gating
    /// or a chain-hold blocker), which the item's own execution rows do not
    /// version. The board runtime cache must not retain such a result.
    pub reads_other_items: bool,
}

/// Wait reason for a project task held back by the ordinal chain.
pub(super) const WAIT_ORDINAL_CHAIN: &str = "ordinal_chain";
/// Wait reason for a pending execution the scheduler has not stamped yet.
pub(super) const WAIT_AWAITING_SCHEDULER: &str = "awaiting_scheduler";

/// Project only pending execution state; never leak an old hold onto a live run.
/// Every pending execution comes back with an engine-stated reason: a
/// dependency, the ordinal chain, a scheduled time, the scheduler's recorded
/// reason, or `awaiting_scheduler` for a runnable row the scheduler has not
/// stamped yet. An unexplained `waiting_dependency` row is logged.
pub(super) fn for_execution(conn: &Connection, execution: Option<&WorkExecution>, now: i64) -> Result<DispatchWait> {
    let Some(execution) = execution else {
        return Ok(DispatchWait::default());
    };
    if !matches!(
        execution.status,
        ExecutionStatus::Queued | ExecutionStatus::Ready | ExecutionStatus::WaitingDependency
    ) {
        return Ok(DispatchWait::default());
    }
    let mut wait = DispatchWait::default();
    if execution.status == ExecutionStatus::WaitingDependency {
        // Reuse dispatch's revision-aware satisfaction rules, including same-PR sequencing.
        // A missing prerequisite row still gates; an empty set means nothing is gating, so
        // fall through to the recorded reason (or unknown) rather than invent a dependency.
        wait.reads_other_items = true;
        if let Some(id) = deps::gating_prereqs_for(conn, &execution.work_item_id)?.first() {
            wait.reason = Some("waiting_dependency".into());
            wait.blocker = blocker_for(conn, id)?;
            return Ok(wait);
        }
    }
    if let Some(epoch) = execution
        .dispatch_not_before
        .as_deref()
        .and_then(|raw| raw.parse::<i64>().ok())
        .filter(|epoch| *epoch > now)
    {
        wait.reason = Some("not_before".into());
        wait.not_before = Some(epoch.to_string());
    } else if let Some(reason) = execution
        .dispatch_wait_reason
        .as_ref()
        .filter(|reason| !reason.trim().is_empty())
    {
        wait.reason = Some(reason.clone());
        let blocker_id: Option<String> = conn.query_row(
            "SELECT dispatch_wait_blocker_id FROM work_executions WHERE id = ?1",
            [&execution.id],
            |row| row.get(0),
        )?;
        if let Some(id) = blocker_id {
            wait.reads_other_items = true;
            // A persisted chain hold is only current while its named writer/reviewer is live.
            // Do not carry its prose or link through the gap before the next scheduler pass.
            if blocker_is_live(conn, &id)? {
                wait.blocker = blocker_for(conn, &id)?;
            } else {
                wait.reason = None;
            }
        }
    }
    if wait.reason.is_none() {
        fill_unstamped(conn, execution, &mut wait)?;
    }
    Ok(wait)
}

/// Explain a pending execution whose scheduler stamp is absent or stale.
fn fill_unstamped(conn: &Connection, execution: &WorkExecution, wait: &mut DispatchWait) -> Result<()> {
    match execution.status {
        ExecutionStatus::WaitingDependency => {
            if let Some(blocker) = ordinal_chain_blocker(conn, &execution.work_item_id)? {
                wait.reason = Some(WAIT_ORDINAL_CHAIN.into());
                wait.blocker = Some(blocker);
            } else {
                tracing::warn!(
                    work_item_id = %execution.work_item_id,
                    execution_id = %execution.id,
                    "queued wait: waiting_dependency execution has no gating prerequisite, is not behind \
                     an earlier project task, and carries no recorded reason",
                );
            }
        }
        _ => wait.reason = Some(WAIT_AWAITING_SCHEDULER.into()),
    }
    Ok(())
}

/// The earlier project task `reconcile_product_executions` keeps this task
/// behind: project tasks run in `(ordinal, created_at, id)` order and only
/// the first one that still accepts an execution is `ready`.
fn ordinal_chain_blocker(conn: &Connection, work_item_id: &str) -> Result<Option<DispatchWaitBlocker>> {
    let Some(task) = query_task(conn, work_item_id)? else {
        return Ok(None);
    };
    let Some(project_id) = task.project_id.as_deref() else {
        return Ok(None);
    };
    if !matches!(task.kind, TaskKind::ProjectTask | TaskKind::Design) {
        return Ok(None);
    }
    let mut stmt = conn.prepare(
        "SELECT id FROM tasks
         WHERE project_id = ?1 AND deleted_at IS NULL AND kind IN ('project_task', 'design')
         ORDER BY COALESCE(ordinal, 9223372036854775807) ASC, created_at ASC, id ASC",
    )?;
    let ids = stmt
        .query_map([project_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        if id == work_item_id {
            return Ok(None);
        }
        if let Some(earlier) = query_task(conn, &id)?
            && task_accepts_execution(&earlier)
        {
            return blocker_for(conn, &earlier.id);
        }
    }
    Ok(None)
}

/// Whether a recorded chain-hold blocker still holds the chain: running, waiting on a human,
/// or claimed (in-flight, cube setup not yet committed to `running`). The scheduler holds on
/// in-flight siblings too, so `query_live_execution_for_work_item` alone would drop that window.
fn blocker_is_live(conn: &Connection, work_item_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_executions
         WHERE work_item_id = ?1 AND status IN ('claimed', 'running', 'waiting_human'))",
        [work_item_id],
        |row| row.get(0),
    )?)
}

fn blocker_for(conn: &Connection, id: &str) -> Result<Option<DispatchWaitBlocker>> {
    if let Some(task) = query_task(conn, id)? {
        return Ok(Some(DispatchWaitBlocker {
            work_item_id: task.id,
            product_id: task.product_id,
            short_id: task.short_id,
            kind: "task".into(),
        }));
    }
    Ok(query_project(conn, id)?.map(|project| DispatchWaitBlocker {
        work_item_id: project.id,
        product_id: project.product_id,
        short_id: project.short_id,
        kind: "project".into(),
    }))
}
