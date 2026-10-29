//! Conservative historical provenance: only evidence on the latest failed
//! execution can explain the current failure. Free-text diagnostics are not gates.
use super::*;

pub(super) fn backfill(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, status, blocked_reason, dispatch_failed_reason, autostart,
                last_status_actor, dispatch_failed_at FROM tasks t
         WHERE NOT EXISTS (SELECT 1 FROM task_status_audit a WHERE a.work_item_id = t.id)",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, status, blocked, dispatch, autostart, actor, dispatch_at) in rows {
        let (basis, observed) = if blocked.as_deref() == Some("worker_failed") {
            failure_evidence(conn, &id)?
        } else {
            ("unknown_legacy".to_owned(), None)
        };
        for (property, value) in [
            ("status", Some(status)),
            ("blocked_reason", blocked),
            ("dispatch_failed_reason", dispatch.clone()),
            ("autostart", Some(autostart.to_string())),
        ] {
            let entry_basis = if property == "blocked_reason" {
                basis.as_str()
            } else if property == "dispatch_failed_reason" && dispatch.as_deref() == Some("deliberate_park") {
                "deliberate_park"
            } else {
                "unknown_legacy"
            };
            let timestamp = if property == "dispatch_failed_reason" && dispatch.is_some() {
                dispatch_at.as_ref()
            } else if property == "blocked_reason" {
                observed.as_ref()
            } else {
                None
            };
            task_status_audit::execute(
                conn,
                entry_basis,
                &actor,
                "INSERT INTO task_status_audit
                 (work_item_id, property, new_value, actor, changed_at, basis, age_is_lower_bound)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    property,
                    value,
                    actor,
                    timestamp.cloned().unwrap_or_else(now_string),
                    entry_basis,
                    timestamp.is_none()
                ],
            )?;
        }
    }
    Ok(())
}

fn failure_evidence(conn: &Connection, id: &str) -> Result<(String, Option<String>)> {
    let latest: Option<(String, String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT id, status, run_done_outcome, finished_at FROM work_executions
         WHERE work_item_id = ?1 ORDER BY CAST(created_at AS INTEGER) DESC, rowid DESC LIMIT 1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((execution, status, outcome, finished)) = latest else {
        return Ok(("unknown_legacy".into(), None));
    };
    if status != "failed" {
        return Ok(("unknown_legacy".into(), None));
    }
    let refusal: Option<String> = conn
        .query_row(
            "SELECT answered_at FROM operator_questions WHERE work_item_id = ?1 AND execution_id = ?2
         AND status = 'answered' AND answered_by = 'human'
         AND json_extract(answer_json, '$.kind') = 'yes_no'
         AND json_extract(answer_json, '$.value') = 0
         ORDER BY CAST(answered_at AS INTEGER) DESC LIMIT 1",
            params![id, execution],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(at) = refusal {
        return Ok(("operator_no".into(), Some(at)));
    }
    let attention: Option<String> = conn
        .query_row(
            "SELECT kind FROM work_attention_items WHERE execution_id = ?1
         AND kind IN ('nudge_breaker_tripped', 'foreign_pr_binding_refused')
         ORDER BY CAST(created_at AS INTEGER) DESC LIMIT 1",
            [&execution],
            |r| r.get(0),
        )
        .optional()?;
    let basis = match attention.as_deref() {
        Some("nudge_breaker_tripped") => "nudge_breaker",
        Some("foreign_pr_binding_refused") => "foreign_pr_binding",
        _ => match outcome.as_deref() {
            Some("blocked") => "declared_blocked",
            Some("delivered") => "delivered_without_pr",
            _ => "unknown_legacy",
        },
    };
    Ok((basis.into(), if basis == "unknown_legacy" { None } else { finished }))
}
