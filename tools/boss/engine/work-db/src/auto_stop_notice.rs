//! The card notice for an engine-initiated stop of an item.
//!
//! The ledger (`task_status_audit`) already records every engine write that
//! clears a task's `autostart` or moves one of its executions to `abandoned`,
//! with a stable reason code. This module turns the unacknowledged rows of
//! that ledger into [`AutoStopNotice`], so an item the engine quietly stopped
//! is marked on its card instead of sitting in Backlog until someone notices.
//!
//! The notice is derived, never stored: the same ledger rows feed `boss task
//! show`, so the badge and the CLI cannot disagree. Only the acknowledgment is
//! stored (`task_auto_stop_ack`, one row per item holding the highest ledger id
//! a human has seen). A stop recorded after that id is a new notice.
//!
//! A row raises the notice only while all of these hold:
//! - the engine wrote it (a normal run-start consumption of `autostart` is
//!   the engine's own write too, so it is excluded by its reason code);
//! - no run has started at or after the stop, so a later start clears it
//!   without hiding a stop that followed an earlier finished run;
//! - no execution is queued or running, so queueing a replacement clears it;
//! - the item is live and not held on purpose (closed, deferred or parked);
//! - for an autostart `1 → 0` write, autostart is still off. Rows written
//!   before the ledger stored `reason_code` still qualify: the code is the
//!   first word of `basis`, the same derivation the recorder uses, so a
//!   pre-enrichment bounce is labelled with its writer and a pre-enrichment
//!   run-start consumption is excluded by that code;
//! - for an abandonment, the execution itself never started a run, and the
//!   cause is not one of [`BENIGN_ABANDON_CAUSES`], where withdrawing the
//!   execution is the intended outcome rather than a stop. Autostart may
//!   still be on: an unreplaced never-started abandonment is a stall even
//!   when the reconciler is supposed to re-mint. Any other cause, including
//!   a write that bypassed the ledger helpers, raises the notice.
//!   Pre-enrichment abandonment rows keep NULL `execution_ever_ran` and
//!   therefore never raise.
//!
//! This module only reads the ledger and the acknowledgment table; it never
//! decides which items are stopped or changes dispatch.
use super::*;
use crate::task_status_audit::{AbandonCause, EXECUTION_ABANDONED_PROPERTY, START_EXECUTION_RUN_BASIS, abandon_cause};
use boss_protocol::AutoStopNotice;
use std::collections::HashMap;

pub(super) const MIGRATION_ID: &str = "20261118_1700_task_auto_stop_ack";

/// Execution statuses that mean an attempt is queued or running.
const IN_FLIGHT_EXECUTION_STATUSES: &str =
    "'queued', 'ready', 'waiting_dependency', 'claimed', 'running', 'waiting_human'";

/// Reason code the property trigger would have stored. Pre-enrichment rows
/// keep NULL `reason_code`; `basis` still names the writer, so the first word
/// of `basis` is the same derivation the recorder uses.
const DERIVED_REASON_SQL: &str = "COALESCE(a.reason_code, substr(a.basis, 1, instr(a.basis || ' ', ' ') - 1))";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_auto_stop_ack (
            work_item_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
            acked_audit_id INTEGER NOT NULL,
            acked_at TEXT NOT NULL,
            acked_by TEXT NOT NULL
        );",
    )?;
    Ok(())
}

/// Abandonments that withdraw an execution on purpose: the work was closed,
/// replaced, retired as unnecessary or held by a human, so the item is not
/// stopped. Everything else raises the notice when nothing replaced the
/// execution.
const BENIGN_ABANDON_CAUSES: [AbandonCause<'static>; 11] = [
    abandon_cause::CLOSED_ITEM_STRANDED,
    abandon_cause::REDUNDANT_DUPLICATE,
    abandon_cause::STALE_REVISION_RETIRED,
    abandon_cause::STALE_GOVERNING_REDISPATCH,
    abandon_cause::PARENT_CLOSED_REVISION,
    abandon_cause::PARENT_CLOSE_SWEEP,
    abandon_cause::RETIRED_SPAWNING_ATTEMPT,
    abandon_cause::DEFERRED,
    abandon_cause::REVIEW_RESERVATION_RELEASED,
    abandon_cause::REVIEW_BATCH_REAPED,
    abandon_cause::REVIEW_BATCH_PR_ENDED,
];

/// The unacknowledged ledger rows that raise a notice, for the items in
/// `?1` (a JSON array of ids), oldest first within an item.
fn candidate_sql() -> String {
    let benign = BENIGN_ABANDON_CAUSES
        .iter()
        .map(|cause| format!("'{}'", cause.code))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT a.work_item_id, a.id, a.property, {derived_reason}, a.detail, a.changed_at
         FROM task_status_audit a
         JOIN tasks t ON t.id = a.work_item_id
         WHERE a.work_item_id IN (SELECT value FROM json_each(?1))
           AND t.deleted_at IS NULL
           AND t.status NOT IN ('done', 'archived')
           AND t.deferred = 0
           AND t.parked_at IS NULL
           AND a.actor = 'engine'
           AND a.id > COALESCE((SELECT k.acked_audit_id FROM task_auto_stop_ack k WHERE k.work_item_id = t.id), 0)
           AND (
                (a.property = 'autostart' AND a.old_value = '1' AND a.new_value = '0'
                 AND t.autostart = 0
                 AND {derived_reason} IS NOT '{run_start}')
                OR
                (a.property = '{abandoned}' AND a.execution_ever_ran = 0
                 AND COALESCE(a.reason_code, '') NOT IN ({benign}))
           )
           AND NOT EXISTS (
                SELECT 1 FROM work_executions live
                WHERE live.work_item_id = t.id AND live.status IN ({in_flight}))
           AND NOT EXISTS (
                SELECT 1 FROM work_executions we
                WHERE we.work_item_id = t.id
                  AND (
                    (we.started_at IS NOT NULL
                     AND CAST(we.started_at AS INTEGER) >= CAST(a.changed_at AS INTEGER))
                    OR EXISTS (
                        SELECT 1 FROM work_runs wr
                        WHERE wr.execution_id = we.id
                          AND wr.started_at IS NOT NULL
                          AND CAST(wr.started_at AS INTEGER) >= CAST(a.changed_at AS INTEGER))))
         ORDER BY a.work_item_id, a.id",
        derived_reason = DERIVED_REASON_SQL,
        run_start = START_EXECUTION_RUN_BASIS,
        abandoned = EXECUTION_ABANDONED_PROPERTY,
        in_flight = IN_FLIGHT_EXECUTION_STATUSES,
        benign = benign,
    )
}

/// The notice of each item in `ids` that has one.
pub(crate) fn notices_among(conn: &Connection, ids: &[&str]) -> Result<HashMap<String, AutoStopNotice>> {
    let mut notices: HashMap<String, AutoStopNotice> = HashMap::new();
    if ids.is_empty() {
        return Ok(notices);
    }
    let mut stmt = conn.prepare_cached(&candidate_sql())?;
    let rows = stmt.query_map([super::query_ensure::id_array(ids)], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    for row in rows {
        let (item, audit_id, property, reason_code, detail, changed_at) = row?;
        let notice = notices.entry(item).or_insert_with(|| AutoStopNotice {
            audit_id,
            reason_code: String::new(),
            detail: None,
            changed_at: String::new(),
            autostart_disabled: false,
            execution_abandoned: false,
        });
        if property == EXECUTION_ABANDONED_PROPERTY {
            notice.execution_abandoned = true;
        } else {
            notice.autostart_disabled = true;
        }
        // Rows come oldest first, so the last one names the latest stop.
        notice.audit_id = audit_id;
        notice.reason_code = reason_code;
        notice.detail = detail;
        notice.changed_at = changed_at;
    }
    Ok(notices)
}

/// Attach each card's notice. Shared by the whole-board read, the incremental
/// card re-read and the single-item projection so all three ship the same
/// notice. A failure is logged and leaves the cards without one.
pub(super) fn attach(conn: &Connection, tasks: &mut [Task], chores: &mut [Task], caller: &str) {
    let ids: Vec<&str> = tasks.iter().chain(chores.iter()).map(|task| task.id.as_str()).collect();
    match notices_among(conn, &ids) {
        Ok(mut notices) => {
            for task in tasks.iter_mut().chain(chores.iter_mut()) {
                task.auto_stop_notice = notices.remove(&task.id);
            }
        }
        Err(err) => tracing::warn!(?err, "{caller}: failed to attach auto_stop_notice; ignoring"),
    }
}

/// Record that a human has seen the item's stops up to ledger row `through`.
/// Clamped to the newest row the item has, so an acknowledgment can never
/// dismiss a stop that has not happened yet, and never lowered, so a stale
/// client cannot bring an acknowledged notice back.
pub(crate) fn acknowledge(
    conn: &Connection,
    work_item_id: &str,
    through: i64,
    actor: &str,
    now: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO task_auto_stop_ack (work_item_id, acked_audit_id, acked_at, acked_by)
         SELECT ?1,
                MIN(?2, (SELECT COALESCE(MAX(id), 0) FROM task_status_audit WHERE work_item_id = ?1)),
                ?3, ?4
         ON CONFLICT(work_item_id) DO UPDATE SET
             acked_audit_id = excluded.acked_audit_id,
             acked_at = excluded.acked_at,
             acked_by = excluded.acked_by
         WHERE excluded.acked_audit_id > task_auto_stop_ack.acked_audit_id",
        params![work_item_id, through.max(0), now, actor],
    )?;
    Ok(())
}
