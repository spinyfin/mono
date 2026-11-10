//! Durable reasons for conflict remediation that has no working fix vehicle.
use super::*;

pub const CONFLICT_WORKSPACE_FAILURE: &str = "workspace_preparation_failed: ";

pub(super) const MIGRATION_ID: &str = "20261109_1500_conflict_workspace_recovery";

/// Per-attempt recovery bookkeeping for a failed conflict workspace: how many
/// times a recovery probe resumed it, the host whose refusal failed it
/// (`NULL` for a local or mechanical-rung failure), the failure context
/// (host plus PR head and base) the last resume saw, whether its revision's
/// failure was already consumed by a resume, and how many consecutive
/// mechanical-rung lease failures it has had.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        (
            "workspace_resumes",
            "ALTER TABLE conflict_resolutions ADD COLUMN workspace_resumes INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "workspace_failure_host",
            "ALTER TABLE conflict_resolutions ADD COLUMN workspace_failure_host TEXT",
        ),
        (
            "workspace_resume_context",
            "ALTER TABLE conflict_resolutions ADD COLUMN workspace_resume_context TEXT",
        ),
        (
            "workspace_failure_consumed",
            "ALTER TABLE conflict_resolutions ADD COLUMN workspace_failure_consumed INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "rung1_lease_failures",
            "ALTER TABLE conflict_resolutions ADD COLUMN rung1_lease_failures INTEGER NOT NULL DEFAULT 0",
        ),
    ] {
        if !super::schema_init::table_has_column(conn, "conflict_resolutions", column)? {
            conn.execute_batch(ddl)?;
        }
    }
    Ok(())
}

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

/// Maximum recoveries of a failed workspace on an unchanged head and base.
/// Past it the attempt stays in its surfaced `workspace_preparation_failed`
/// state until the head or base moves.
pub const MAX_UNCHANGED_WORKSPACE_RESUMES: i64 = 3;

/// Consecutive mechanical-rung lease failures tolerated before the attempt
/// fails with Cube's last error and is surfaced.
pub const MAX_RUNG1_LEASE_FAILURES: i64 = 5;

pub(crate) const WORKSPACE_RECOVERED_ARCHIVE_REASON: &str = "workspace preparation recovered";

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

fn workspace_failure_candidate(conn: &rusqlite::Connection, attempt_id: &str) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT t.dispatch_failed_error, t.id FROM conflict_resolutions current
             JOIN conflict_resolutions prior ON prior.id = (
                 SELECT p.id FROM conflict_resolutions p
                  WHERE p.work_item_id = current.work_item_id AND p.revision_task_id IS NOT NULL
                    AND ((current.status IN ('pending', 'running') AND p.id = current.id)
                      OR (current.status = 'abandoned' AND current.failure_reason = 'churn_threshold_exceeded'
                          AND p.head_sha_before IS current.head_sha_before))
                  ORDER BY p.created_at DESC, p.id DESC LIMIT 1)
             JOIN tasks t ON t.id = prior.revision_task_id
             WHERE current.id = ?1
               AND t.dispatch_failed_reason IN ('cube_workspace_positioning_failed', 'cube_workspace_lease_failed')
               AND t.dispatch_failed_error IS NOT NULL
               AND t.archived_reason IS NOT ?2
               AND prior.workspace_failure_consumed = 0",
            params![attempt_id, WORKSPACE_RECOVERED_ARCHIVE_REASON],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
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

    /// Classify an attempt as a workspace-preparation failure when its
    /// revision's persisted dispatch failure is a Cube lease or positioning
    /// stage. Reads the persisted stage, never Cube's diagnostic text. For a
    /// churn-abandoned attempt only the most recent attempt on the same head
    /// decides, so an old positioning failure cannot reclassify a churn stop
    /// whose later attempts died for other reasons.
    pub fn retain_conflict_revision_workspace_failure(&self, attempt_id: &str) -> Result<bool> {
        if workspace_failure_candidate(&*self.connect()?, attempt_id)?.is_none() {
            return Ok(false);
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some((error, revision)) = workspace_failure_candidate(&tx, attempt_id)? else {
            return Ok(false);
        };
        // The host that produced the refusal is the host of the revision's
        // last execution (every pre-start selection records its host; a NULL
        // host predates remote selection and so ran locally). With no
        // execution at all the host is unknown: keep the churn stop and its
        // diagnostic rather than guess a host to probe.
        let host: Option<Option<String>> = tx
            .query_row(
                "SELECT host_id FROM work_executions WHERE work_item_id = ?1
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                [&revision],
                |row| row.get(0),
            )
            .optional()?;
        let Some(host) = host else {
            return Ok(false);
        };
        let remote = host.filter(|host| host != boss_protocol::LOCAL_HOST_ID);
        tx.execute(
            "UPDATE conflict_resolutions SET status = 'failed', failure_reason = ?2,
            finished_at = COALESCE(finished_at, ?3), resolved_by_rung = NULL, revision_task_id = ?4,
            workspace_failure_host = ?5 WHERE id = ?1",
            params![
                attempt_id,
                format!("{CONFLICT_WORKSPACE_FAILURE}{error}"),
                now_string(),
                revision,
                remote
            ],
        )?;
        surface_conflict_stop(&tx, attempt_id)?;
        tx.commit()?;
        Ok(true)
    }

    /// Preserve Cube's full diagnostic, independent of worker liveness. Repeated
    /// observations of the same failed attempt do not mint another terminal state.
    pub fn fail_conflict_workspace(&self, attempt_id: &str, error: &str) -> Result<Option<ConflictResolution>> {
        self.fail_conflict_workspace_on_host(attempt_id, error, None)
    }

    /// As [`Self::fail_conflict_workspace`], recording the host whose refusal
    /// failed the attempt so a recovery probe asks that host and not this
    /// engine's own Cube. `None` and the local host id mean "local".
    pub fn fail_conflict_workspace_on_host(
        &self,
        attempt_id: &str,
        error: &str,
        host_id: Option<&str>,
    ) -> Result<Option<ConflictResolution>> {
        let failed =
            self.mark_conflict_resolution_failed(attempt_id, &format!("{CONFLICT_WORKSPACE_FAILURE}{error}"))?;
        let remote = host_id.filter(|host| *host != boss_protocol::LOCAL_HOST_ID);
        self.connect()?.execute(
            "UPDATE conflict_resolutions SET workspace_failure_host = ?2
             WHERE id = ?1 AND status = 'failed' AND failure_reason LIKE ?3 || '%'",
            params![attempt_id, remote, CONFLICT_WORKSPACE_FAILURE],
        )?;
        Ok(failed)
    }

    /// The remote host whose refusal failed this attempt, if any.
    pub fn conflict_workspace_failure_host(&self, attempt_id: &str) -> Result<Option<String>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT workspace_failure_host FROM conflict_resolutions WHERE id = ?1",
                [attempt_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
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

    /// Resumes already spent on the attempt's current failure episode, and the
    /// failure context that was current at the last resume.
    fn workspace_resume_state(&self, attempt_id: &str) -> Result<(i64, Option<String>)> {
        Ok(self.connect()?.query_row(
            "SELECT workspace_resumes, workspace_resume_context FROM conflict_resolutions WHERE id = ?1",
            [attempt_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    /// The stable identity of a failure episode: the host whose refusal failed
    /// the attempt plus the PR head and base the refusal was about. Diagnostic
    /// text is deliberately not part of it, so wording drift cannot reopen the
    /// budget.
    fn workspace_failure_context(
        &self,
        attempt: &ConflictResolution,
        head: Option<&str>,
        base: Option<&str>,
    ) -> Result<String> {
        let host = self.conflict_workspace_failure_host(&attempt.id)?;
        Ok(format!(
            "{}|{}|{}",
            host.as_deref().unwrap_or(boss_protocol::LOCAL_HOST_ID),
            head.or(attempt.head_sha_before.as_deref()).unwrap_or_default(),
            base.or(attempt.base_sha_at_trigger.as_deref()).unwrap_or_default(),
        ))
    }

    /// Resumes spent in the current failure episode, and its context identity.
    fn workspace_resume_budget(
        &self,
        attempt: &ConflictResolution,
        head: Option<&str>,
        base: Option<&str>,
    ) -> Result<(i64, String)> {
        let context = self.workspace_failure_context(attempt, head, base)?;
        let (spent, last) = self.workspace_resume_state(&attempt.id)?;
        let spent = if last.as_deref().is_some_and(|last| last != context) {
            0
        } else {
            spent
        };
        Ok((spent, context))
    }

    /// Whether a recovery probe that succeeds would be refused because the
    /// failure episode (same host, same PR head and base) has already used its
    /// resume budget. The probe cannot observe the bookmark or repository state
    /// Cube refused, so only a change of host, head or base reopens it.
    pub fn workspace_resume_paused(&self, attempt_id: &str, head: Option<&str>, base: Option<&str>) -> Result<bool> {
        let Some(attempt) = self
            .get_conflict_resolution(attempt_id)?
            .filter(conflict_workspace_failed)
        else {
            return Ok(false);
        };
        Ok(self.workspace_resume_budget(&attempt, head, base)?.0 >= MAX_UNCHANGED_WORKSPACE_RESUMES)
    }

    /// Called only after Cube has successfully prepared the workspace again on
    /// the host that failed. Keep the old revision and its failure records
    /// until that positive evidence exists; then retire it and reuse the
    /// attempt without consuming the churn cap.
    ///
    /// The resume budget lives on the attempt and is spent atomically by every
    /// resume, with or without a revision. A changed failing host, PR head or
    /// base reopens it. Once spent, the attempt stays surfaced as paused.
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
        let (spent, context) = self.workspace_resume_budget(&attempt, head, base)?;
        if spent >= MAX_UNCHANGED_WORKSPACE_RESUMES {
            self.note_workspace_recovery_paused(&attempt)?;
            return Ok(None);
        }
        if let Some(revision) = &attempt.revision_task_id {
            if self.is_conflict_revision_running_or_queued(revision)? {
                return Ok(None);
            }
            // Archiving is a no-op for a revision that is already archived,
            // done, in review or deleted; the consumed marker below is what
            // keeps its failure from being replayed.
            self.close_moot_revision_task(
                revision,
                ArchiveProvenance::new(
                    ARCHIVE_MECHANISM_CONFLICT_WATCH_SUPERSESSION,
                    WORKSPACE_RECOVERED_ARCHIVE_REASON,
                ),
            )?;
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(revision) = &attempt.revision_task_id {
            tx.execute(
                "UPDATE conflict_resolutions SET workspace_failure_consumed = 1 WHERE revision_task_id = ?1",
                [revision],
            )?;
        }
        let rows = tx.execute(
            "UPDATE conflict_resolutions SET status = 'pending', failure_reason = NULL,
                 finished_at = NULL, revision_task_id = NULL, workspace_failure_host = NULL,
                 workspace_resumes = ?5 + 1, workspace_resume_context = ?6, rung1_lease_failures = 0,
                 head_sha_before = COALESCE(?3, head_sha_before), base_sha_at_trigger = COALESCE(?4, base_sha_at_trigger)
             WHERE id = ?1 AND status = 'failed' AND failure_reason = ?2",
            params![attempt_id, attempt.failure_reason, head, base, spent, context],
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

    /// Tell whoever follows the surfaced advice that automatic recovery is
    /// paused and what lifts the pause, on both the attention and the card.
    /// Returns whether a row changed.
    pub fn note_workspace_recovery_paused(&self, attempt: &ConflictResolution) -> Result<bool> {
        self.note_workspace_recovery(
            &attempt.id,
            &format!(
                "Automatic recovery is paused after {MAX_UNCHANGED_WORKSPACE_RESUMES} unsuccessful resumes on the \
                 same host and PR head and base. It resumes when the PR head or base moves or the failure \
                 occurs on a different host."
            ),
        )
    }

    /// Append `note` (why recovery is not happening right now) to the surfaced
    /// stop. Idempotent: repeated identical notes write nothing. Returns
    /// whether the card or the attention text actually changed.
    pub fn note_workspace_recovery(&self, attempt_id: &str, note: &str) -> Result<bool> {
        let Some(attempt) = self
            .get_conflict_resolution(attempt_id)?
            .filter(conflict_workspace_failed)
        else {
            return Ok(false);
        };
        let reason = attempt.failure_reason.as_deref().unwrap_or_default();
        let detail = format!("Conflict remediation stopped: {reason}\n\n{note}");
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let card = tx.execute(
            "UPDATE tasks SET blocked_detail = ?2 WHERE id = ?1 AND blocked_attempt_id = ?3
               AND blocked_reason = 'merge_conflict' AND blocked_detail IS NOT ?2",
            params![attempt.work_item_id, detail, attempt.id],
        )?;
        let attention = tx.execute(
            "UPDATE work_attention_items SET body_markdown = ?3 WHERE work_item_id = ?1 AND status = 'open'
               AND kind = ?2 AND body_markdown IS NOT ?3",
            params![
                attempt.work_item_id,
                CONFLICT_REMEDIATION_STOPPED_ATTENTION_KIND,
                format!("PR {}\n\n{detail}", attempt.pr_url)
            ],
        )?;
        tx.commit()?;
        Ok(card + attention > 0)
    }

    /// Record one more consecutive mechanical-rung lease failure on the
    /// attempt and return the new count.
    pub fn record_rung1_lease_failure(&self, attempt_id: &str) -> Result<i64> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE conflict_resolutions SET rung1_lease_failures = rung1_lease_failures + 1 WHERE id = ?1",
            [attempt_id],
        )?;
        Ok(conn
            .query_row(
                "SELECT rung1_lease_failures FROM conflict_resolutions WHERE id = ?1",
                [attempt_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// A lease succeeded: the failures are no longer consecutive.
    pub fn clear_rung1_lease_failures(&self, attempt_id: &str) -> Result<()> {
        self.connect()?.execute(
            "UPDATE conflict_resolutions SET rung1_lease_failures = 0 WHERE id = ?1 AND rung1_lease_failures != 0",
            [attempt_id],
        )?;
        Ok(())
    }
}
