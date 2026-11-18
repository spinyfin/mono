//! Immediate evidence for an unexplained wait, independent of heartbeat grace.
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use super::*;

pub(super) const MIGRATION_ID: &str = "20261118_0600_execution_state_transitions";

/// How long one anomaly stays suppressed. An anomaly still present after this
/// long is reported again, so a wait that outlives a log rotation stays visible.
const REPORT_TTL_SECS: i64 = 3600;

/// Execution-level transition times, which `task_status_audit` cannot give: a
/// claim, a claim release or a wait-reason change never moves `tasks.status`.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS execution_state_transitions (
            execution_id TEXT PRIMARY KEY REFERENCES work_executions(id) ON DELETE CASCADE,
            changed_at INTEGER NOT NULL,
            age_is_lower_bound INTEGER NOT NULL DEFAULT 0
         );
         INSERT OR IGNORE INTO execution_state_transitions (execution_id, changed_at, age_is_lower_bound)
            SELECT id, CAST(strftime('%s','now') AS INTEGER), 1 FROM work_executions;
         CREATE TRIGGER IF NOT EXISTS execution_state_transitions_insert AFTER INSERT ON work_executions
         BEGIN
            INSERT INTO execution_state_transitions (execution_id, changed_at)
                VALUES (NEW.id, CAST(strftime('%s','now') AS INTEGER))
                ON CONFLICT(execution_id) DO UPDATE SET changed_at = excluded.changed_at, age_is_lower_bound = 0;
         END;
         CREATE TRIGGER IF NOT EXISTS execution_state_transitions_update
         AFTER UPDATE OF status, dispatch_wait_reason ON work_executions
         WHEN OLD.status IS NOT NEW.status OR OLD.dispatch_wait_reason IS NOT NEW.dispatch_wait_reason
         BEGIN
            INSERT INTO execution_state_transitions (execution_id, changed_at)
                VALUES (NEW.id, CAST(strftime('%s','now') AS INTEGER))
                ON CONFLICT(execution_id) DO UPDATE SET changed_at = excluded.changed_at, age_is_lower_bound = 0;
         END;",
    )?;
    Ok(())
}

/// One item whose classification came out unclassified. `execution_id` is the
/// row the classifier consulted, and `None` means it consulted none: the log
/// never substitutes another execution.
pub(super) struct Unclassified<'a> {
    pub work_item_id: &'a str,
    pub execution_id: Option<&'a str>,
    pub execution_status: Option<&'a str>,
}

type AnomalyKey = (String, Option<String>, Option<String>);

static REPORTED: LazyLock<Mutex<HashMap<AnomalyKey, i64>>> = LazyLock::new(Mutex::default);

/// Whether this anomaly has not been reported within the TTL. Keys not seen
/// for a full TTL are pruned, so the set cannot grow without bound.
fn first_report(item: &Unclassified<'_>, now: i64) -> bool {
    let key = (
        item.work_item_id.to_owned(),
        item.execution_id.map(str::to_owned),
        item.execution_status.map(str::to_owned),
    );
    let Ok(mut reported) = REPORTED.lock() else {
        return true;
    };
    reported.retain(|_, last| now.saturating_sub(*last) < REPORT_TTL_SECS);
    reported.insert(key, now).is_none()
}

/// Log each distinct anomaly once, on first exposure. Read as a set so logging
/// does not add a query per board card, and only for anomalies not yet reported.
pub(super) fn log_unclassified(conn: &Connection, items: &[Unclassified<'_>]) {
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let fresh: Vec<(&str, Option<&str>)> = items
        .iter()
        .filter(|item| first_report(item, now))
        .map(|item| (item.work_item_id, item.execution_id))
        .collect();
    if fresh.is_empty() {
        return;
    }
    if let Err(error) = log_rows(conn, &fresh, now) {
        tracing::warn!(?error, "failed to record unexplained dispatch wait state");
    }
}

fn log_rows(conn: &Connection, items: &[(&str, Option<&str>)], now: i64) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "SELECT json_extract(item.value, '$[0]'), e.id, e.status,
                a.changed_at, a.age_is_lower_bound, x.changed_at, x.age_is_lower_bound,
                json_object(
                    'task_status', t.status, 'task_kind', t.kind,
                    'autostart', t.autostart, 'deferred', t.deferred,
                    'human_driven', t.human_driven, 'parked_at', t.parked_at,
                    'blocked_reason', t.blocked_reason, 'blocked_detail', t.blocked_detail,
                    'dispatch_failed_reason', t.dispatch_failed_reason,
                    'project_id', t.project_id, 'parent_task_id', t.parent_task_id,
                    'pr_url', t.pr_url, 'repo_remote_url', t.repo_remote_url,
                    'review_required_state', t.review_required_state,
                    'execution_kind', e.kind, 'execution_created_at', e.created_at,
                    'execution_started_at', e.started_at, 'execution_finished_at', e.finished_at,
                    'dispatch_wait_reason', e.dispatch_wait_reason,
                    'dispatch_wait_since', e.dispatch_wait_since,
                    'dispatch_wait_blocker_id', e.dispatch_wait_blocker_id,
                    'dispatch_not_before', e.dispatch_not_before,
                    'dispatch_not_before_source', e.dispatch_not_before_source,
                    'cube_lease_id', e.cube_lease_id, 'last_error', e.last_error)
         FROM json_each(?1) item
         LEFT JOIN tasks t ON t.id = json_extract(item.value, '$[0]')
         LEFT JOIN work_executions e ON e.id = json_extract(item.value, '$[1]')
         LEFT JOIN execution_state_transitions x ON x.execution_id = e.id
         LEFT JOIN task_status_audit a ON a.id = (
             SELECT id FROM task_status_audit
             WHERE work_item_id = t.id AND property = 'status' ORDER BY id DESC LIMIT 1)",
    )?;
    let mut rows = stmt.query([serde_json::to_string(items)?])?;
    while let Some(row) = rows.next()? {
        let work_item_id: String = row.get(0)?;
        let execution_id: Option<String> = row.get(1)?;
        let execution_status: Option<String> = row.get(2)?;
        let changed_at: Option<String> = row.get(3)?;
        let age_is_lower_bound: Option<bool> = row.get(4)?;
        let state_fields: String = row.get(7)?;
        let execution_changed_at: Option<i64> = row.get(5)?;
        let execution_age_is_lower_bound: Option<bool> = row.get(6)?;
        let execution_state_age_secs = execution_changed_at.map(|epoch| now.saturating_sub(epoch).max(0));
        let task_state_age_secs = changed_at
            .as_deref()
            .and_then(|value| value.parse::<i64>().ok())
            .map(|epoch| now.saturating_sub(epoch).max(0));
        tracing::error!(
            work_item_id,
            execution_id = execution_id.as_deref().unwrap_or("(none)"),
            execution_status = execution_status.as_deref().unwrap_or("(none)"),
            execution_state_changed_at = ?execution_changed_at,
            execution_state_age_secs,
            execution_state_age_is_lower_bound = ?execution_age_is_lower_bound,
            task_state_changed_at = ?changed_at,
            task_state_age_secs,
            task_state_age_is_lower_bound = ?age_is_lower_bound,
            state_fields,
            "dispatch wait unclassified: exposed waiting state without a reason",
        );
    }
    Ok(())
}
