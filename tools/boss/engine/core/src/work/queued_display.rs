use super::*;
use boss_engine_utils::iso8601::{format_elapsed_ago, format_epoch_iso8601};
use boss_protocol::QueuedDisplay;

/// The engine selects precedence once; both the full tree and runtime patches
/// carry the resulting prose. The app only paints it and the absolute tooltip.
pub(super) fn for_execution(
    conn: &Connection,
    execution: Option<&WorkExecution>,
    wait: &super::queued_wait::DispatchWait,
    now: i64,
    expires_at: &mut Option<i64>,
) -> Result<Option<QueuedDisplay>> {
    let Some(execution) = execution.filter(|e| {
        matches!(
            e.status,
            ExecutionStatus::Queued | ExecutionStatus::Ready | ExecutionStatus::WaitingDependency
        )
    }) else {
        return Ok(None);
    };
    let mut tooltip_times = Vec::new();
    let hold = if let Some(deadline) = wait.not_before.as_deref().and_then(|s| s.parse::<i64>().ok()) {
        tooltip_times.push(format!("Next dispatch attempt: {}", format_epoch_iso8601(deadline)));
        let remaining = deadline - now;
        let next_change = if remaining < 60 {
            deadline
        } else if remaining <= 60 {
            now + 1
        } else {
            deadline - ((remaining - 1) / 60) * 60
        };
        expire_at(expires_at, next_change);
        let relative = if remaining < 60 {
            "in less than a minute".to_owned()
        } else {
            format!("in {} min", (remaining + 59) / 60)
        };
        if dispatch_helpers::dispatch_retry_at_for_execution(execution, now).is_some() {
            format!("Retrying dispatch — next attempt {relative}")
        } else {
            format!("Queued — not before {relative}")
        }
    } else {
        let reason = match wait.reason.as_deref() {
            Some("waiting_dependency") => {
                let label = wait.blocker.as_ref().and_then(|b| {
                    b.short_id
                        .map(|id| format!("{}{id}", if b.kind == "project" { "P" } else { "T" }))
                });
                format!("waiting for {}", label.as_deref().unwrap_or("a dependency"))
            }
            Some("dispatch_paused") => "dispatch paused".into(),
            Some("automation_paused") => "automation paused".into(),
            Some("pool_exhausted") => "worker pool full".into(),
            None | Some("unknown" | "not_before" | "") => "reason unknown".into(),
            Some(reason) => reason.into(),
        };
        let mut hold = format!("Queued — {reason}");
        if wait.reason.is_some()
            && wait.reason == execution.dispatch_wait_reason
            && let Some(since) = execution
                .dispatch_wait_since
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
        {
            expire_elapsed(expires_at, since, now);
            if let Some(relative) = format_elapsed_ago(since, now) {
                hold.push_str(&format!(" ({relative})"));
                tooltip_times.push(format!("Waiting since: {}", format_epoch_iso8601(since)));
            }
        }
        hold
    };

    // Include retries of the same execution as well as replacement executions.
    // Select the immediately preceding outcome, not an arbitrary older failure.
    let previous: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT COALESCE(r.status, e.status), COALESCE(r.finished_at, e.finished_at)
         FROM work_executions e LEFT JOIN work_runs r ON r.execution_id = e.id
         WHERE e.work_item_id = ?1 AND (e.id != ?2 OR r.id IS NOT NULL)
           AND (e.created_at < ?3 OR (e.created_at = ?3 AND e.id <= ?2))
         ORDER BY e.created_at DESC, e.id DESC, r.created_at DESC, r.id DESC LIMIT 1",
            params![execution.work_item_id, execution.id, execution.created_at],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let mut text = hold;
    if let Some((outcome, finished)) =
        previous.filter(|(status, _)| matches!(status.as_str(), "orphaned" | "failed" | "cancelled"))
    {
        let verb = if outcome == "failed" {
            "failed".to_owned()
        } else {
            format!("was {outcome}")
        };
        let mut restart = format!("Restarting after the previous run {verb}");
        if let Some(epoch) = finished.as_deref().and_then(|s| s.parse::<i64>().ok()) {
            expire_elapsed(expires_at, epoch, now);
            if let Some(relative) = format_elapsed_ago(epoch, now) {
                restart.push_str(&format!(" {relative}"));
            }
            tooltip_times.push(format!("Previous run {verb} at {}", format_epoch_iso8601(epoch)));
        }
        text = format!("{restart}. {text}");
    }
    let tooltip = if tooltip_times.is_empty() {
        text.clone()
    } else {
        format!("{text}\n{}", tooltip_times.join("\n"))
    };
    Ok(Some(QueuedDisplay { text, tooltip }))
}

fn expire_at(expires_at: &mut Option<i64>, deadline: i64) {
    *expires_at = Some(expires_at.map_or(deadline, |old| old.min(deadline)));
}

fn expire_elapsed(expires_at: &mut Option<i64>, epoch: i64, now: i64) {
    // Elapsed phrases can change at a minute boundary. A conservative minute
    // expiry also covers the hour/day/month/year boundaries of the formatter.
    let next = if epoch > now {
        epoch
    } else {
        now + 60 - (now - epoch) % 60
    };
    expire_at(expires_at, next);
}

/// Add the new runtime input to epoch invalidation for existing databases.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS board_runtime_epoch_run_finished
         AFTER UPDATE OF finished_at ON work_runs
         WHEN OLD.finished_at IS NOT NEW.finished_at
         BEGIN
             INSERT INTO board_runtime_epoch(work_item_id, epoch)
             SELECT work_item_id, 1 FROM work_executions WHERE id = NEW.execution_id
             ON CONFLICT(work_item_id) DO UPDATE SET epoch = epoch + 1;
         END",
    )?;
    Ok(())
}

impl WorkDb {
    /// Heartbeat projection, including future deadlines that the dispatchable
    /// queue deliberately excludes. Callers publish only changed displays.
    pub(crate) fn queued_displays(&self, now: i64) -> Result<Vec<(String, String, QueuedDisplay)>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let items = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT t.product_id, t.id FROM tasks t
                 JOIN work_executions e ON e.work_item_id = t.id
                 WHERE t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived')
                   AND e.status IN ('queued', 'ready', 'waiting_dependency')",
            )?;
            collect_rows(stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?)?
        };
        let mut displays = Vec::new();
        for (product, id) in items {
            let runtime = runtime_projection::compute_task_runtime(&tx, &id, &mut 0, now)?.runtime;
            if let Some(display) = runtime.queued_display {
                displays.push((product, id, display));
            }
        }
        Ok(displays)
    }
}
