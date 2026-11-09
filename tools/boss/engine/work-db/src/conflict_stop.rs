//! Durable reasons for conflict remediation that has no working fix vehicle.
use super::*;

pub const CONFLICT_WORKSPACE_FAILURE: &str = "workspace_preparation_failed: ";

pub fn conflict_workspace_failed(attempt: &ConflictResolution) -> bool {
    attempt.status == "failed"
        && attempt
            .failure_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with(CONFLICT_WORKSPACE_FAILURE))
}

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
    let detail =
        if reason == CHURN_GUARD_REASON {
            let last_error: Option<String> = conn.query_row(
            "SELECT t.dispatch_failed_error FROM conflict_resolutions cr JOIN tasks t ON t.id = cr.revision_task_id
             WHERE cr.work_item_id = ?1 AND t.dispatch_failed_error IS NOT NULL
             ORDER BY cr.created_at DESC, cr.id DESC LIMIT 1",
            [&attempt.work_item_id], |row| row.get(0),
        ).optional()?;
            format!(
                "Conflict remediation stopped after repeated unsuccessful attempts.{}",
                last_error
                    .map(|error| format!(" Last workspace or worker error: {error}"))
                    .unwrap_or_default()
            )
        } else {
            format!("Conflict remediation stopped: {reason}")
        };
    let updated = crate::task_status_audit::execute(
        conn,
        "surface_conflict_stop",
        "engine",
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict',
             blocked_detail = ?2, blocked_attempt_id = ?3, last_status_actor = 'engine', updated_at = ?4
         WHERE id = ?1 AND pr_url = ?5 AND deleted_at IS NULL
           AND (status = 'in_review' OR (status = 'blocked' AND blocked_reason = 'merge_conflict'))",
        params![attempt.work_item_id, detail, attempt.id, now_string(), attempt.pr_url],
    )?;
    if updated == 0 {
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

#[work_db_boundary::instrument]
impl WorkDb {
    /// Prediction telemetry cannot hide a stopped remediation episode.
    pub fn latest_actionable_conflict_resolution(&self, work_item_id: &str) -> Result<Option<ConflictResolution>> {
        let conn = self.connect()?;
        let id: Option<String> = conn.query_row(
            "SELECT id FROM conflict_resolutions WHERE work_item_id = ?1 AND event_source != 'speculative_predicted'
             ORDER BY created_at DESC, id DESC LIMIT 1", [work_item_id], |row| row.get(0),
        ).optional()?;
        id.map(|id| query_conflict_resolution(&conn, &id))
            .transpose()
            .map(Option::flatten)
    }

    /// Repair older terminal rows that predate durable blocked details. This
    /// also makes a lost notification harmless without re-raising the same
    /// attention on every sweep.
    pub fn ensure_conflict_stop_visible(&self, attempt_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let missing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM conflict_resolutions cr JOIN tasks t ON t.id = cr.work_item_id
             WHERE cr.id = ?1 AND cr.status IN ('failed', 'abandoned')
               AND t.pr_url = cr.pr_url AND t.deleted_at IS NULL
               AND (t.status = 'in_review' OR (t.status = 'blocked' AND t.blocked_reason = 'merge_conflict'))
               AND ((t.blocked_detail IS NULL OR t.blocked_attempt_id IS NOT cr.id) OR NOT EXISTS(
                 SELECT 1 FROM work_attention_items a WHERE a.work_item_id = t.id
                   AND a.kind = CASE WHEN cr.failure_reason LIKE 'lockfile_regeneration_failed:%' THEN ?3 ELSE ?2 END
                   AND a.status = 'open')))",
            params![
                attempt_id,
                CONFLICT_REMEDIATION_STOPPED_ATTENTION_KIND,
                LOCKFILE_REGENERATION_FAILED_ATTENTION_KIND
            ],
            |row| row.get(0),
        )?;
        if missing {
            surface_conflict_stop(&tx, attempt_id)?;
        }
        tx.commit()?;
        Ok(missing)
    }

    /// Before structured pre-start failures were attached to the attempt,
    /// dispatch retained the preparation stage and full error on the revision.
    /// Use that persisted stage, never text matching on Cube's diagnostic.
    pub fn retain_conflict_revision_workspace_failure(&self, attempt_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let error: Option<(String, String)> = tx
            .query_row(
                "SELECT t.dispatch_failed_error, t.id FROM conflict_resolutions current
             JOIN conflict_resolutions prior ON prior.work_item_id = current.work_item_id
             JOIN tasks t ON t.id = prior.revision_task_id
             WHERE current.id = ?1
               AND ((current.status IN ('pending', 'running') AND prior.id = current.id)
                 OR (current.status = 'abandoned' AND current.failure_reason = 'churn_threshold_exceeded'
                     AND prior.head_sha_before IS current.head_sha_before))
               AND t.dispatch_failed_reason IN ('cube_workspace_positioning_failed', 'cube_workspace_lease_failed')
               AND t.dispatch_failed_error IS NOT NULL
             ORDER BY prior.created_at DESC, prior.id DESC LIMIT 1",
                [attempt_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((error, revision)) = error else {
            return Ok(false);
        };
        tx.execute(
            "UPDATE conflict_resolutions SET status = 'failed', failure_reason = ?2,
            finished_at = COALESCE(finished_at, ?3), resolved_by_rung = NULL, revision_task_id = ?4 WHERE id = ?1",
            params![
                attempt_id,
                format!("{CONFLICT_WORKSPACE_FAILURE}{error}"),
                now_string(),
                revision
            ],
        )?;
        surface_conflict_stop(&tx, attempt_id)?;
        tx.commit()?;
        Ok(true)
    }

    /// Preserve Cube's full diagnostic, independent of worker liveness. Repeated
    /// observations of the same failed attempt do not mint another terminal state.
    pub fn fail_conflict_workspace(&self, attempt_id: &str, error: &str) -> Result<Option<ConflictResolution>> {
        self.mark_conflict_resolution_failed(attempt_id, &format!("{CONFLICT_WORKSPACE_FAILURE}{error}"))
    }

    pub fn conflict_resolution_for_revision(&self, revision_id: &str) -> Result<Option<ConflictResolution>> {
        let conn = self.connect()?;
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM conflict_resolutions WHERE revision_task_id = ?1
             ORDER BY created_at DESC, id DESC LIMIT 1",
                [revision_id],
                |row| row.get(0),
            )
            .optional()?;
        id.map(|id| query_conflict_resolution(&conn, &id))
            .transpose()
            .map(Option::flatten)
    }

    /// Called only after Cube has successfully prepared the workspace again.
    /// Keep the old revision and its failure records until that positive evidence
    /// exists; then retire it and reuse the attempt without consuming the churn cap.
    pub fn resume_conflict_workspace(
        &self,
        attempt_id: &str,
        head: Option<&str>,
        base: Option<&str>,
    ) -> Result<Option<ConflictResolution>> {
        let Some(attempt) = self
            .get_conflict_resolution(attempt_id)?
            .filter(conflict_workspace_failed)
        else {
            return Ok(None);
        };
        if let Some(revision) = &attempt.revision_task_id {
            if self.is_conflict_revision_running_or_queued(revision)? {
                return Ok(None);
            }
            self.close_moot_revision_task(
                revision,
                ArchiveProvenance::new(
                    ARCHIVE_MECHANISM_CONFLICT_WATCH_SUPERSESSION,
                    "workspace preparation recovered",
                ),
            )?;
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let rows = tx.execute(
            "UPDATE conflict_resolutions SET status = 'pending', failure_reason = NULL,
                 finished_at = NULL, revision_task_id = NULL,
                 head_sha_before = COALESCE(?3, head_sha_before), base_sha_at_trigger = COALESCE(?4, base_sha_at_trigger)
             WHERE id = ?1 AND status = 'failed' AND failure_reason = ?2",
            params![attempt_id, attempt.failure_reason, head, base],
        )?;
        if rows == 0 {
            return Ok(None);
        }
        tx.execute("UPDATE tasks SET blocked_detail = NULL WHERE id = ?1 AND blocked_attempt_id = ?2 AND blocked_reason = 'merge_conflict'",
            params![attempt.work_item_id, attempt_id])?;
        dispatch_helpers::resolve_attention_kind_in_tx(
            &tx,
            &attempt.work_item_id,
            CONFLICT_REMEDIATION_STOPPED_ATTENTION_KIND,
        )?;
        let resumed = query_conflict_resolution(&tx, attempt_id)?;
        tx.commit()?;
        Ok(resumed)
    }
}
