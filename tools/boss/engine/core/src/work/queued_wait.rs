use super::*;
use boss_protocol::DispatchWaitBlocker;

#[derive(Default)]
pub(super) struct DispatchWait {
    pub reason: Option<String>,
    pub not_before: Option<String>,
    pub blocker: Option<DispatchWaitBlocker>,
}

/// Project only pending execution state; never leak an old hold onto a live run.
pub(super) fn for_execution(conn: &Connection, execution: Option<&WorkExecution>) -> Result<DispatchWait> {
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
        .filter(|epoch| *epoch > boss_engine_utils::epoch_time::now_epoch_secs())
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
            // A persisted chain hold is only current while its named writer/reviewer is live.
            // Do not carry its prose or link through the gap before the next scheduler pass.
            if blocker_is_live(conn, &id)? {
                wait.blocker = blocker_for(conn, &id)?;
            } else {
                wait.reason = None;
            }
        }
    }
    Ok(wait)
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
