//! Immediate evidence for an unexplained wait, independent of heartbeat grace.
use super::*;

/// Read anomalies as a set so logging does not add a query per board card.
/// An explicit execution id is the row the projection classified; absent ids
/// resolve the latest execution, including a terminal run on an idle card.
pub(super) fn log_unclassified(conn: &Connection, items: &[(&str, Option<&str>)]) {
    if items.is_empty() {
        return;
    }
    if let Err(error) = log_rows(conn, items) {
        tracing::warn!(?error, "failed to record unexplained dispatch wait state");
    }
}

fn log_rows(conn: &Connection, items: &[(&str, Option<&str>)]) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "SELECT json_extract(item.value, '$[0]'), e.id, e.status,
                a.changed_at, a.age_is_lower_bound,
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
         LEFT JOIN work_executions e ON e.id = COALESCE(json_extract(item.value, '$[1]'),
             (SELECT latest.id FROM work_executions latest
              WHERE latest.work_item_id = json_extract(item.value, '$[0]')
              ORDER BY latest.created_at DESC, latest.id DESC LIMIT 1))
         LEFT JOIN task_status_audit a ON a.id = (
             SELECT id FROM task_status_audit
             WHERE work_item_id = t.id AND property = 'status' ORDER BY id DESC LIMIT 1)",
    )?;
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let mut rows = stmt.query([serde_json::to_string(items)?])?;
    while let Some(row) = rows.next()? {
        let work_item_id: String = row.get(0)?;
        let execution_id: Option<String> = row.get(1)?;
        let execution_status: Option<String> = row.get(2)?;
        let changed_at: Option<String> = row.get(3)?;
        let age_is_lower_bound: Option<bool> = row.get(4)?;
        let state_fields: String = row.get(5)?;
        let task_state_age_secs = changed_at
            .as_deref()
            .and_then(|value| value.parse::<i64>().ok())
            .map(|epoch| now.saturating_sub(epoch).max(0));
        tracing::error!(
            work_item_id,
            execution_id = execution_id.as_deref().unwrap_or("(none)"),
            execution_status = execution_status.as_deref().unwrap_or("(none)"),
            task_state_changed_at = ?changed_at,
            task_state_age_secs,
            task_state_age_is_lower_bound = ?age_is_lower_bound,
            state_fields,
            "dispatch wait unclassified: exposed waiting state without a reason",
        );
    }
    Ok(())
}
