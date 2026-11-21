//! Staging for executions ended by decision (`cancelled` / `abandoned`).
//!
//! Callers that supply [`CancelProvenance`] stamp `cancel_source` /
//! `cancel_actor` / `cancel_reason` in the same `UPDATE` that moves the
//! status, then call [`stage_decision_terminal`] so the `ExecutionTerminal`
//! event and the matching engine-audit record flush after commit. Existing
//! writers that omit provenance still succeed: the columns stay nullable and
//! no schema trigger requires them.

use boss_protocol::{CancelProvenance, ExecutionStatus};

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
        let provenance = CancelProvenance::new(boss_protocol::CancelSource::Bossctl, "operator", "wrong repo").unwrap();
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
