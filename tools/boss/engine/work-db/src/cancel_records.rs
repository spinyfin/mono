//! The durable record of who cancelled an execution and why.
//!
//! Every path that terminalizes an execution as `cancelled` calls
//! [`stage_cancel_provenance`] inside its transaction. That writes one row to
//! `execution_cancel_events`, so the record commits or rolls back with the
//! cancel itself, and stages an [`Event::ExecutionCancelled`] notification
//! that is published only after the commit.
//!
//! This crate cannot reach `engine-audit.log` or the dispatch timeline: both
//! live above it. The engine's publisher (`boss_engine::cancel_timeline`)
//! reads the undelivered rows through [`WorkDb::pending_cancel_records`],
//! writes the audit record and, for a cancel whose caller did not put it on
//! the timeline itself, the `execution_cancelled` dispatch event, then calls
//! [`WorkDb::acknowledge_cancel_record`]. A row is delivered at least once: a
//! crash between the write and the acknowledgement repeats it, carrying the
//! original [`ExecutionCancelRecord::cancelled_at`].

use serde::{Deserialize, Serialize};

use super::*;

/// Migration id for the `execution_cancel_events` table.
pub(super) const MIGRATION_ID: &str = "20261118_1530_execution_cancel_events";

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS execution_cancel_events (
    execution_id TEXT PRIMARY KEY NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,
    event_json TEXT NOT NULL,
    delivered INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS execution_cancel_events_pending_idx
ON execution_cancel_events(execution_id) WHERE delivered = 0";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

/// One committed cancel, as stored in `execution_cancel_events.event_json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ExecutionCancelRecord {
    pub execution_id: String,
    pub work_item_id: String,
    /// The status that was terminalized (the row says `cancelled` afterwards).
    pub from_status: String,
    /// Who decided the cancel, in the `tasks.last_status_actor` vocabulary.
    pub actor: String,
    pub reason: String,
    /// Whether the cancel also moved the owning row (demoted it from Doing
    /// and/or stopped it in Backlog).
    pub work_item_moved: bool,
    pub queued_only: bool,
    /// Pid of the process that issued the cancel, when it arrived over RPC.
    pub peer_pid: Option<i32>,
    /// False when the caller writes the dispatch-timeline event itself.
    pub on_timeline: bool,
    /// Epoch seconds at which the cancel was written.
    pub cancelled_at: String,
}

/// Everything one cancel records. Built by every path that terminalizes an
/// execution as `cancelled` and handed to [`stage_cancel_provenance`].
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct CancelProvenance<'a> {
    pub execution_id: &'a str,
    pub work_item_id: &'a str,
    pub from_status: ExecutionStatus,
    pub actor: StatusActor,
    pub reason: &'a str,
    pub work_item_moved: bool,
    pub queued_only: bool,
    pub peer_pid: Option<i32>,
    /// The caller writes the dispatch-timeline event itself.
    pub caller_records_timeline: bool,
}

/// Record one cancel in the cancelling transaction `tx`, and stage the
/// notification that wakes the publisher once that transaction commits. One
/// funnel keeps the vocabulary identical across every cancel path: who
/// (`actor`), why (`reason`), the status that was terminalized
/// (`from_status`), and whether the cancel also moved the owning row
/// (`work_item_moved`).
pub(crate) fn stage_cancel_provenance(
    tx: &Connection,
    pending: &mut PendingEvents,
    cancel: CancelProvenance<'_>,
) -> Result<()> {
    let record = ExecutionCancelRecord {
        execution_id: cancel.execution_id.to_owned(),
        work_item_id: cancel.work_item_id.to_owned(),
        from_status: cancel.from_status.as_str().to_owned(),
        actor: cancel.actor.as_str().to_owned(),
        reason: cancel.reason.to_owned(),
        work_item_moved: cancel.work_item_moved,
        queued_only: cancel.queued_only,
        peer_pid: cancel.peer_pid,
        on_timeline: !cancel.caller_records_timeline,
        cancelled_at: now_string(),
    };
    // An execution is cancelled once; REPLACE only matters for a row that was
    // put back by hand and cancelled again, where the later cancel stands.
    tx.execute(
        "INSERT OR REPLACE INTO execution_cancel_events (execution_id, event_json, delivered) VALUES (?1, ?2, 0)",
        params![record.execution_id, serde_json::to_string(&record)?],
    )?;
    pending.push(Event::ExecutionCancelled {
        execution_id: record.execution_id,
        task_id: record.work_item_id,
    });
    Ok(())
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Committed cancels the publisher has not yet written to the audit log
    /// and dispatch timeline, oldest execution id first.
    pub fn pending_cancel_records(&self) -> Result<Vec<ExecutionCancelRecord>> {
        let conn = self.connect()?;
        let mut stmt =
            conn.prepare("SELECT event_json FROM execution_cancel_events WHERE delivered = 0 ORDER BY execution_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Mark `execution_id`'s cancel record as published.
    pub fn acknowledge_cancel_record(&self, execution_id: &str) -> Result<()> {
        self.connect()?.execute(
            "UPDATE execution_cancel_events SET delivered = 1 WHERE execution_id = ?1",
            [execution_id],
        )?;
        Ok(())
    }
}
