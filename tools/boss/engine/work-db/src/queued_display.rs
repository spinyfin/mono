use std::collections::HashMap;

use super::queued_wait::ReviewNaming;
use super::*;
use boss_engine_storage_values::dispatch_wait::{
    DISPATCH_WAIT_REASON_AWAITING_DISPATCH, DISPATCH_WAIT_REASON_FAILURE_BACKOFF, DISPATCH_WAIT_REASON_NOT_BEFORE,
    DISPATCH_WAIT_REASON_UNCLASSIFIED,
};
use boss_engine_utils::iso8601::format_elapsed_ago;
use boss_protocol::{QueuedDisplay, QueuedTooltipTime};

/// A predecessor execution that finished this long before the replacement
/// was created is not treated as the cause of a restart. Same-execution
/// retries skip this bound.
const RESTART_PREFIX_MAX_AGE_SECS: i64 = 60 * 60;

/// Cached heartbeat projection: reuse while the item's epoch and
/// `expires_at` still match, so the 15s scan does not recompute every
/// pending runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedDisplaySnapshot {
    pub product_id: String,
    pub display: QueuedDisplay,
    epoch: i64,
    expires_at: Option<i64>,
    cacheable: bool,
}

/// The engine selects precedence once; both the full tree and runtime patches
/// carry the resulting prose. The app paints `text` and localizes each
/// `tooltip_times` epoch.
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
        tooltip_times.push(QueuedTooltipTime {
            label: "Next dispatch attempt:".into(),
            epoch: deadline,
        });
        let remaining = deadline - now;
        let next_change = if remaining < 60 {
            deadline
        } else if remaining == 60 {
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
        let lead = match (wait.reason.as_deref(), wait.label.as_deref()) {
            (Some(DISPATCH_WAIT_REASON_NOT_BEFORE), _) => "Queued".to_owned(),
            (Some(DISPATCH_WAIT_REASON_FAILURE_BACKOFF), Some(label)) => label.to_owned(),
            (_, Some(label)) if !label.trim().is_empty() => format!("Queued — {label}"),
            _ => "Unexpected state — the engine sent no reason for this wait".to_owned(),
        };
        format!("{lead} — next attempt {relative}")
    } else {
        let mut hold = match wait.label.as_deref().filter(|label| !label.trim().is_empty()) {
            None => "Unexpected state — the engine sent no reason for this wait".to_owned(),
            Some(label)
                if matches!(
                    wait.reason.as_deref(),
                    None | Some("" | "unknown" | DISPATCH_WAIT_REASON_UNCLASSIFIED)
                ) =>
            {
                format!("Unexpected state — {label}")
            }
            Some(label) => format!("Queued — {label}"),
        };
        if execution.status == ExecutionStatus::Ready
            && let Some(since) = execution
                .dispatch_wait_since
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
        {
            expire_elapsed(expires_at, since, now);
            if let Some(relative) = format_elapsed_ago(since, now) {
                hold.push_str(&format!(" ({relative})"));
                tooltip_times.push(QueuedTooltipTime {
                    label: "Waiting since:".into(),
                    epoch: since,
                });
            }
        }
        hold
    };

    let mut next_up_text = if wait.reason.as_deref() == Some(DISPATCH_WAIT_REASON_AWAITING_DISPATCH) {
        "Next up".to_owned()
    } else {
        format!("Next up — {}", hold.strip_prefix("Queued — ").unwrap_or(&hold))
    };
    let mut text = hold;
    if let Some((outcome, finished)) = previous_abnormal_outcome(conn, execution)? {
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
            tooltip_times.push(QueuedTooltipTime {
                label: format!("Previous run {verb} at"),
                epoch,
            });
        }
        text = format!("{restart}. {text}");
        next_up_text = format!("{restart}. {next_up_text}");
    }
    Ok(Some(QueuedDisplay {
        text,
        next_up_text: Some(next_up_text),
        tooltip_times,
    }))
}

/// Distinct predecessor executions use the execution row's terminal outcome.
/// A pane-spawn run is marked `completed` while the execution stays live, so
/// joining `work_runs` would hide later orphan/fail/cancel. Runs apply only
/// to retries of the current pending execution.
fn previous_abnormal_outcome(conn: &Connection, execution: &WorkExecution) -> Result<Option<(String, Option<String>)>> {
    let current_run: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT status, finished_at FROM work_runs
             WHERE execution_id = ?1
             ORDER BY created_at DESC, id DESC LIMIT 1",
            params![execution.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((status, finished_at)) = current_run {
        return Ok(abnormal_outcome(status, finished_at));
    }
    let previous: Option<(String, Option<String>, String)> = conn
        .query_row(
            "SELECT e.status, e.finished_at, e.kind
             FROM work_executions e
             WHERE e.work_item_id = ?1 AND e.id != ?2
               AND (e.created_at < ?3 OR (e.created_at = ?3 AND e.id < ?2))
             ORDER BY e.created_at DESC, e.id DESC LIMIT 1",
            params![execution.work_item_id, execution.id, execution.created_at],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((status, finished_at, kind)) = previous else {
        return Ok(None);
    };
    if kind != execution.kind.as_str() {
        return Ok(None);
    }
    let Some(finished) = finished_at.as_deref().and_then(|s| s.parse::<i64>().ok()) else {
        return Ok(None);
    };
    let created = execution.created_at.parse::<i64>().unwrap_or(finished);
    if created.saturating_sub(finished) > RESTART_PREFIX_MAX_AGE_SECS {
        return Ok(None);
    }
    Ok(abnormal_outcome(status, finished_at))
}

fn abnormal_outcome(status: String, finished_at: Option<String>) -> Option<(String, Option<String>)> {
    matches!(status.as_str(), "orphaned" | "failed" | "cancelled").then_some((status, finished_at))
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
    /// Unexpired, same-epoch, cacheable snapshots from `previous` are reused.
    ///
    /// The whole pass is one read snapshot and writes nothing, so it is safe
    /// on a read-only connection and must never take the write lock the
    /// connection's default transaction would.
    pub fn queued_displays(
        &self,
        now: i64,
        previous: &HashMap<String, QueuedDisplaySnapshot>,
    ) -> Result<HashMap<String, QueuedDisplaySnapshot>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let items = {
            let mut stmt = tx.prepare(
                // Driven from the pending executions, which are few, through
                // the status index. Left to choose, the planner starts from
                // `tasks` and reads every non-deleted task on each beat.
                //
                // Whether the task is still on the board is a result column,
                // not a `WHERE` term: a condition on `tasks` alone lets the
                // planner pre-filter the join with a Bloom filter, and it
                // builds one by reading every task.
                "SELECT DISTINCT t.product_id, t.id, COALESCE(ep.epoch, 0),
                        t.deleted_at IS NULL AND t.status NOT IN ('done', 'archived')
                 FROM work_executions e INDEXED BY work_executions_ready_idx
                 CROSS JOIN tasks t ON t.id = e.work_item_id
                 LEFT JOIN board_runtime_epoch ep ON ep.work_item_id = t.id
                 WHERE e.status IN ('queued', 'ready', 'waiting_dependency')",
            )?;
            let rows: Vec<(String, String, i64, bool)> = collect_rows(stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            })?)?;
            rows.into_iter()
                .filter(|(_, _, _, on_board)| *on_board)
                .map(|(product_id, id, epoch, _)| (product_id, id, epoch))
                .collect::<Vec<_>>()
        };
        let mut displays = HashMap::new();
        for (product_id, id, epoch) in items {
            if let Some(prev) = previous.get(&id)
                && prev.cacheable
                && prev.epoch == epoch
                && prev.expires_at.is_none_or(|expires| now < expires)
            {
                displays.insert(id, prev.clone());
                continue;
            }
            let computed = dispatch_helpers::compute_task_runtime(&tx, &id, &mut 0, now, ReviewNaming::Inline)?;
            if let Some(display) = computed.runtime.queued_display {
                displays.insert(
                    id,
                    QueuedDisplaySnapshot {
                        product_id,
                        display,
                        epoch,
                        expires_at: computed.expires_at,
                        cacheable: computed.cacheable,
                    },
                );
            }
        }
        Ok(displays)
    }
}
