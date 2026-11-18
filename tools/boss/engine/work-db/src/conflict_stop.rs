//! Durable reasons for conflict remediation that has no working fix vehicle.
use super::*;

/// Record why a failed or abandoned conflict attempt stopped.
///
/// The parent task's `blocked_detail` and `blocked_attempt_id` name the
/// terminal attempt, and one open attention item carries the same text. A
/// parent held by a different blocker keeps its own reason but still gets the
/// attention.
pub(crate) fn surface_conflict_stop(conn: &rusqlite::Transaction<'_>, attempt_id: &str) -> Result<()> {
    let Some(attempt) = query_conflict_resolution(conn, attempt_id)? else {
        return Ok(());
    };
    if !matches!(attempt.status.as_str(), "failed" | "abandoned") {
        return Ok(());
    }
    let reason = attempt
        .failure_reason
        .as_deref()
        .unwrap_or("Conflict remediation stopped without a result");
    let detail = if reason == CHURN_GUARD_REASON {
        let last_error: Option<String> = conn
            .query_row(
                "SELECT t.dispatch_failed_error FROM conflict_resolutions cr JOIN tasks t ON t.id = cr.revision_task_id
                 WHERE cr.work_item_id = ?1 AND t.dispatch_failed_error IS NOT NULL
                 ORDER BY cr.created_at DESC, cr.id DESC LIMIT 1",
                [&attempt.work_item_id],
                |row| row.get(0),
            )
            .optional()?;
        format!(
            "Conflict remediation stopped after repeated unsuccessful attempts.{}",
            last_error
                .map(|error| format!(" Last workspace or worker error: {error}"))
                .unwrap_or_default()
        )
    } else {
        format!("Conflict remediation stopped: {reason}")
    };
    // The scalar blocked reason is only overwritten when the parent is waiting
    // on the merge conflict itself. A higher-priority blocker (dependency,
    // review feedback) keeps its reason, but the stop is still raised below.
    crate::task_status_audit::execute(
        conn,
        "surface_conflict_stop",
        "engine",
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict',
             blocked_detail = ?2, blocked_attempt_id = ?3, last_status_actor = 'engine', updated_at = ?4
         WHERE id = ?1 AND pr_url = ?5 AND deleted_at IS NULL
           AND (status = 'in_review' OR (status = 'blocked' AND blocked_reason = 'merge_conflict'))",
        params![attempt.work_item_id, detail, attempt.id, now_string(), attempt.pr_url],
    )?;
    let parent_awaits_pr: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND pr_url = ?2 AND deleted_at IS NULL
                         AND status IN ('in_review', 'blocked'))",
        params![attempt.work_item_id, attempt.pr_url],
        |row| row.get(0),
    )?;
    if !parent_awaits_pr {
        return Ok(());
    }
    workitems::upsert_refreshing_work_item_attention_in_tx(
        conn,
        &attempt.work_item_id,
        if reason.starts_with("lockfile_regeneration_failed:") {
            LOCKFILE_REGENERATION_FAILED_ATTENTION_KIND
        } else {
            CONFLICT_REMEDIATION_STOPPED_ATTENTION_KIND
        },
        "Conflict remediation stopped",
        &format!("PR {}\n\n{detail}", attempt.pr_url),
    )?;
    Ok(())
}

fn conflict_stop_missing(conn: &rusqlite::Connection, attempt_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM conflict_resolutions cr JOIN tasks t ON t.id = cr.work_item_id
         WHERE cr.id = ?1 AND cr.status IN ('failed', 'abandoned')
           AND t.pr_url = cr.pr_url AND t.deleted_at IS NULL
           AND t.status IN ('in_review', 'blocked')
           AND (((t.status = 'in_review' OR t.blocked_reason = 'merge_conflict')
                 AND (t.blocked_detail IS NULL OR t.blocked_attempt_id IS NOT cr.id))
             OR NOT EXISTS(
             SELECT 1 FROM work_attention_items a WHERE a.work_item_id = t.id
               AND a.kind = CASE WHEN cr.failure_reason LIKE 'lockfile_regeneration_failed:%' THEN ?3 ELSE ?2 END
               AND a.status = 'open')))",
        params![
            attempt_id,
            CONFLICT_REMEDIATION_STOPPED_ATTENTION_KIND,
            LOCKFILE_REGENERATION_FAILED_ATTENTION_KIND
        ],
        |row| row.get(0),
    )?)
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// The newest attempt that is not telemetry: speculative predictions and
    /// deferral records cannot hide a stopped remediation episode.
    pub fn latest_actionable_conflict_resolution(&self, work_item_id: &str) -> Result<Option<ConflictResolution>> {
        let conn = self.connect()?;
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM conflict_resolutions
                  WHERE work_item_id = ?1 AND event_source != 'speculative_predicted'
                    AND status != 'deferred' AND deferral_end_reason IS NULL
                  ORDER BY created_at DESC, id DESC LIMIT 1",
                [work_item_id],
                |row| row.get(0),
            )
            .optional()?;
        id.map(|id| query_conflict_resolution(&conn, &id))
            .transpose()
            .map(Option::flatten)
    }

    /// Repair older terminal rows that predate durable blocked details. This
    /// also makes a lost notification harmless without re-raising the same
    /// attention on every sweep.
    pub fn ensure_conflict_stop_visible(&self, attempt_id: &str) -> Result<bool> {
        // Read first: the steady state is "already visible", and taking the
        // write lock on every detection pass would serialise needlessly.
        if !conflict_stop_missing(&*self.connect()?, attempt_id)? {
            return Ok(false);
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let missing = conflict_stop_missing(&tx, attempt_id)?;
        if missing {
            surface_conflict_stop(&tx, attempt_id)?;
        }
        tx.commit()?;
        Ok(missing)
    }
}
