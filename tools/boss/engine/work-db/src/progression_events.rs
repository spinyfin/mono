//! What ends a progression run's cooldown early, and the one-off reset of the
//! calibrated cooldown value that predates it.
//!
//! The cooldown in [`crate::progression_candidates`] is a *fallback*: how long
//! an item waits for progression to look again when nothing has happened. Two
//! events make the wait pointless, so a run that has one is no longer holding
//! its item in cooldown, and the item is a candidate again at the next sweep
//! (still subject to every other clause, the live-work exclusion included):
//!
//! - **A filed action reached a terminal state.** The run created a revision or
//!   a prerequisite (`tasks.created_via = 'progression:<run id>'`) and that task
//!   is now done (merged or completed), archived, deleted, or failed
//!   (`blocked: worker_failed`). Whether the fix worked is the next run's
//!   question; waiting out a timer for it only delays the answer.
//! - **The owner answered the run's question.** The answer is on the item's
//!   brief and in the next run's packet, and nothing else acts on it.
//!
//! Events are read from existing rows. Terminal actions filed by any run on
//! the item release cooldowns evaluated before that event.
use super::*;
use boss_engine_storage_values::progression::METADATA_KEY_COOLDOWN_SECS;

pub(super) const RESET_CALIBRATED_COOLDOWN_MIGRATION_ID: &str = "20261120_2240_progression_cooldown_reset";

/// SQL for "progression run `run` of item `item` has an event that ends its
/// cooldown early". `run` and `item` are aliases of the caller's execution and
/// task rows. Callers put it after the cheap time comparison, so it is only
/// evaluated for runs that are still inside their cooldown.
pub(crate) fn run_ended_early_sql(run: &str, item: &str) -> String {
    let prefix = boss_protocol::CREATED_VIA_PROGRESSION_PREFIX;
    format!(
        "(EXISTS (SELECT 1 FROM operator_questions oq
                  WHERE oq.work_item_id = {item}.id AND oq.execution_id = {run}.id AND oq.status = 'answered')
          OR EXISTS (SELECT 1 FROM tasks filed
                     JOIN work_executions filer ON filed.created_via = '{prefix}' || filer.id
                     WHERE filer.work_item_id = {item}.id AND filer.kind = 'progression'
                       AND (filer.id = {run}.id OR
                            MAX(COALESCE(CAST(filed.deleted_at AS INTEGER), 0),
                                COALESCE((SELECT MAX(CAST(event.changed_at AS INTEGER))
                                          FROM task_status_audit event WHERE event.work_item_id = filed.id
                                            AND ((event.property = 'status' AND event.new_value IN ('done', 'archived', 'blocked'))
                                              OR (event.property = 'blocked_reason' AND event.new_value = 'worker_failed'))), 0))
                              > COALESCE(CAST({run}.finished_at AS INTEGER), CAST({run}.created_at AS INTEGER)))
                       AND (filed.deleted_at IS NOT NULL
                            OR filed.status IN ('done', 'archived')
                            OR (filed.status = 'blocked' AND filed.blocked_reason = 'worker_failed'))))"
    )
}

/// The owner answered a question a progression run asked: waive the stuck-age
/// grace once, so the item is a candidate at the next sweep instead of two hours
/// after the answer moved it back to review. Questions asked by anything else
/// are the owner's decision for that worker, not a prompt for progression.
pub(crate) fn waive_grace_after_answer(
    tx: &rusqlite::Transaction<'_>,
    item_id: &str,
    asking_execution_id: &str,
) -> Result<()> {
    let kind: Option<String> = tx
        .query_row(
            "SELECT kind FROM work_executions WHERE id = ?1",
            [asking_execution_id],
            |row| row.get(0),
        )
        .optional()?;
    if kind.as_deref() == Some(ExecutionKind::Progression.as_str()) {
        tx.execute(
            // Stamped by SQLite when the statement runs, like every other waiver,
            // so it is never earlier than the ledger rows the answer just wrote.
            "UPDATE tasks SET progression_grace_waived_at = strftime('%s','now')
             WHERE id = ?1 AND deleted_at IS NULL AND status NOT IN ('done', 'archived')",
            params![item_id],
        )?;
    }
    Ok(())
}

/// Drop the cooldown the daily calibration published before the cooldown stopped
/// being calibrated (three times the longest revision-to-review time, about 52
/// hours), and the cached report that recorded it. Without the row the views
/// use the 6-hour default; startup configuration republishes settings when the
/// report is missing, including an explicitly configured cooldown.
pub(super) fn migrate_reset_calibrated_cooldown(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM metadata WHERE key IN (?1, ?2)",
        params![METADATA_KEY_COOLDOWN_SECS, crate::progression_calibration::REPORT_KEY],
    )?;
    Ok(())
}
