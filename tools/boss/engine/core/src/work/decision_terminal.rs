//! Staging for executions ended by decision (`cancelled` / `abandoned`).
//!
//! Every write that moves an execution to one of those statuses stamps
//! `cancel_source` / `cancel_actor` / `cancel_reason` in the same `UPDATE`
//! (the schema-56 trigger refuses the write otherwise) and then calls
//! [`stage_decision_terminal`], which stages the `ExecutionTerminal` event
//! and the matching `engine-audit.log` record to flush after commit.

use boss_protocol::{CancelProvenance, CancelSource, ExecutionStatus};

use super::*;

/// Caller context only the RPC layer knows, carried into the audit record so
/// `execution_cancelled` includes the request options and socket peer.
#[derive(Debug, Clone, Default)]
pub struct DecisionAuditContext {
    /// `Some` for a cancel that arrived as `CancelExecution`.
    pub queued_only: Option<bool>,
    /// The socket peer that sent the request, when there was one.
    pub peer_pid: Option<i32>,
    /// Chain root whose parent PR merged, when this cancel is a merge-poller
    /// parent-merge retirement.
    pub chain_root_id: Option<String>,
    /// Execution status immediately before the merge-poller cancel.
    pub prior_status: Option<String>,
}

/// The audit event name for a decision-terminal `status`.
fn audit_event_name(status: &ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Cancelled => "execution_cancelled",
        ExecutionStatus::Abandoned => "execution_abandoned",
        other => unreachable!("`{other}` is not a decision-terminal execution status"),
    }
}

/// The audit payload identifies the execution, status transition, caller,
/// cause, request options and socket peer.
pub(crate) fn decision_audit_payload(
    execution_id: &str,
    work_item_id: &str,
    from_status: &str,
    to_status: &ExecutionStatus,
    provenance: &CancelProvenance,
    context: &DecisionAuditContext,
) -> serde_json::Value {
    serde_json::json!({
        "execution_id": execution_id,
        "work_item_id": work_item_id,
        "from_status": from_status,
        "to_status": to_status.as_str(),
        "reason": &provenance.reason,
        "source": provenance.source.as_str(),
        "actor": &provenance.actor,
        "queued_only": context.queued_only.unwrap_or(false),
        "peer_pid": context.peer_pid,
        "chain_root_id": context.chain_root_id,
        "prior_status": context.prior_status,
    })
}

/// A non-terminal execution an UPDATE is about to end, captured beforehand
/// so the audit record can name its prior status.
#[derive(Debug, Clone)]
pub(crate) struct DoomedExecution {
    pub id: String,
    pub work_item_id: String,
    pub from_status: String,
}

impl DoomedExecution {
    pub(crate) fn new(id: impl Into<String>, work_item_id: impl Into<String>, from_status: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            work_item_id: work_item_id.into(),
            from_status: from_status.into(),
        }
    }
}

/// Stage the terminal event and the audit record for an execution this
/// transaction just moved to `to_status` (`cancelled` or `abandoned`).
pub(crate) fn stage_decision_terminal(
    pending: &mut PendingEvents,
    conn: &Connection,
    doomed: &DoomedExecution,
    to_status: &ExecutionStatus,
    provenance: &CancelProvenance,
    context: &DecisionAuditContext,
) -> Result<()> {
    stage_execution_terminal(pending, conn, &doomed.id, &doomed.work_item_id)?;
    pending.push_audit(
        audit_event_name(to_status),
        decision_audit_payload(
            &doomed.id,
            &doomed.work_item_id,
            &doomed.from_status,
            to_status,
            provenance,
            context,
        ),
    );
    Ok(())
}

/// Read `(id, work_item_id, status)` for the rows `where_sql` selects. The
/// caller then runs its own `UPDATE` over the same predicate inside the same
/// transaction, so the captured set is exactly the set that changes.
pub(crate) fn capture_doomed(
    conn: &Connection,
    where_sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<DoomedExecution>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, work_item_id, status FROM work_executions WHERE {where_sql}"
    ))?;
    let rows = stmt
        .query_map(params, |row| {
            Ok(DoomedExecution {
                id: row.get(0)?,
                work_item_id: row.get(1)?,
                from_status: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Stage terminal + audit for every captured row. Callers pass the rows they
/// captured with [`capture_doomed`] before their bulk `UPDATE`.
pub(crate) fn stage_doomed(
    pending: &mut PendingEvents,
    conn: &Connection,
    doomed: &[DoomedExecution],
    to_status: &ExecutionStatus,
    provenance: &CancelProvenance,
) -> Result<()> {
    for row in doomed {
        stage_decision_terminal(
            pending,
            conn,
            row,
            to_status,
            provenance,
            &DecisionAuditContext::default(),
        )?;
    }
    Ok(())
}

/// Abandon a governing execution whose worker is gone so `request_execution`
/// can mint a fresh ready row. Stamps [`CancelSource::StaleGoverningExecution`].
pub(crate) fn abandon_stale_governing(
    pending: &mut PendingEvents,
    conn: &Connection,
    existing: &WorkExecution,
    work_item_id: &str,
    latest_id: Option<&str>,
    live_id: Option<&str>,
) -> Result<()> {
    tracing::info!(
        work_item_id = %work_item_id,
        governing_execution_id = %existing.id,
        latest_execution_id = ?latest_id,
        live_execution_id = ?live_id,
        governing_status = %existing.status,
        decision = "abandon_stale_and_redispatch",
        "dispatch_decision: governing execution not live (worker gone) — \
         abandoning it and creating a fresh ready execution",
    );
    let provenance = CancelProvenance::engine(
        CancelSource::StaleGoverningExecution,
        "request_execution",
        format!(
            "execution {} governed work item {work_item_id} in status `{}` but its worker is gone; \
             abandoned so a fresh ready execution can replace it",
            existing.id, existing.status
        ),
    );
    conn.execute(
        "UPDATE work_executions
         SET status = 'abandoned',
             finished_at = COALESCE(finished_at, ?2),
             cancel_source = ?3,
             cancel_actor = ?4,
             cancel_reason = ?5
         WHERE id = ?1",
        params![
            existing.id,
            now_string(),
            provenance.source.as_str(),
            provenance.actor,
            provenance.reason
        ],
    )?;
    stage_decision_terminal(
        pending,
        conn,
        &DoomedExecution::new(&existing.id, work_item_id, existing.status.as_str()),
        &ExecutionStatus::Abandoned,
        &provenance,
        &DecisionAuditContext::default(),
    )?;
    Ok(())
}

/// These sites do not publish `ExecutionTerminal`; only provenance is staged.
pub(crate) fn stage_doomed_audit_only(
    pending: &mut PendingEvents,
    doomed: &[DoomedExecution],
    to_status: &ExecutionStatus,
    provenance: &CancelProvenance,
) {
    for row in doomed {
        pending.push_audit(
            audit_event_name(to_status),
            decision_audit_payload(
                &row.id,
                &row.work_item_id,
                &row.from_status,
                to_status,
                provenance,
                &DecisionAuditContext::default(),
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_payload_extends_the_original_cancel_event() {
        let provenance = CancelProvenance::new(CancelSource::Bossctl, "operator", "wrong repo").unwrap();
        let payload = decision_audit_payload(
            "exec_1",
            "task_1",
            "running",
            &ExecutionStatus::Cancelled,
            &provenance,
            &DecisionAuditContext {
                queued_only: Some(false),
                peer_pid: Some(42),
                chain_root_id: None,
                prior_status: None,
            },
        );
        // Every field the pre-provenance event carried is still present.
        for key in [
            "execution_id",
            "work_item_id",
            "from_status",
            "reason",
            "queued_only",
            "peer_pid",
        ] {
            assert!(payload.get(key).is_some(), "lost original audit field `{key}`");
        }
        assert_eq!(payload["reason"], "wrong repo");
        assert_eq!(payload["source"], "bossctl");
        assert_eq!(payload["actor"], "operator");
        assert_eq!(payload["to_status"], "cancelled");
    }
}
