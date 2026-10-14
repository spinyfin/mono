use super::*;
use boss_engine_recovery::execution_bookmark::ExecutionBookmark;

impl WorkDb {
    /// Complete an engine-driven rewrite by checkpointing its baseline
    /// bookmarks and their database provenance before reporting success.
    pub(crate) async fn checkpoint_rewritten_baselines(&self, work_item_id: &str) -> Result<()> {
        use boss_engine_recovery::execution_bookmark::{LocalJj, checkpoint_baseline, is_missing_pointer_error};
        for execution in self.list_executions(Some(work_item_id))? {
            let Some(record) = self.execution_bookmark_optional(&execution.id)? else {
                continue;
            };
            // Engine-direct cube rewrites run on the engine host. Other
            // hosts' repositories are independent and were not rewritten.
            if record.host_id != "local" {
                continue;
            }
            let base = match checkpoint_baseline(&LocalJj, &record).await {
                Ok(base) => base,
                Err(err) if is_missing_pointer_error(&err) => continue,
                Err(err) => return Err(err),
            };
            self.update_execution_bookmark_base(&execution.id, &base)?;
        }
        Ok(())
    }

    pub(crate) fn record_execution_restore_report(
        &self,
        execution_id: &str,
        report: &boss_engine_recovery::execution_bookmark::RestoreReport,
    ) -> Result<()> {
        self.connect()?.execute("INSERT INTO execution_restore_reports (execution_id, report) VALUES (?1, ?2) ON CONFLICT(execution_id) DO UPDATE SET report = excluded.report", params![execution_id, serde_json::to_string(report)?])?;
        Ok(())
    }

    pub(crate) fn execution_restore_report(
        &self,
        execution_id: &str,
    ) -> Result<Option<boss_engine_recovery::execution_bookmark::RestoreReport>> {
        let report: Option<String> = self
            .connect()?
            .query_row(
                "SELECT report FROM execution_restore_reports WHERE execution_id = ?1",
                [execution_id],
                |row| row.get(0),
            )
            .optional()?;
        report
            .map(|text| serde_json::from_str(&text).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn terminal_bookmark_executions(&self, grace: i64, lookback: i64) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM work_executions WHERE started_at IS NOT NULL
            AND status IN ('completed', 'failed', 'cancelled', 'orphaned', 'abandoned')
            AND CAST(finished_at AS INTEGER) <= unixepoch('now') - ?1
            AND CAST(finished_at AS INTEGER) >= unixepoch('now') - ?2
            AND NOT EXISTS (SELECT 1 FROM execution_bookmarks
                WHERE execution_id = work_executions.id AND retired_at IS NOT NULL)",
        )?;
        let ids = stmt
            .query_map(params![grace, lookback], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| query_execution(&conn, &id)?.context("terminal execution disappeared"))
            .collect()
    }
    pub(crate) fn record_bookmark_recovery(&self, execution_id: &str, predecessor: &str, has_work: bool) -> Result<()> {
        let changed = self.connect()?.execute(
            "UPDATE execution_bookmarks SET recovered_from = ?2, recovered_work = ?3 WHERE execution_id = ?1",
            params![execution_id, predecessor, has_work],
        )?;
        anyhow::ensure!(changed == 1, "recovery execution bookmark record missing");
        Ok(())
    }

    pub(crate) fn bookmark_recovery(&self, execution_id: &str) -> Result<Option<(String, bool)>> {
        Ok(self.connect()?.query_row("SELECT recovered_from, recovered_work FROM execution_bookmarks WHERE execution_id = ?1 AND recovered_from IS NOT NULL", [execution_id], |row| Ok((row.get(0)?, row.get(1)?))).optional()?)
    }
    /// A retired row for the same execution (a deliberate fresh start after
    /// its own pointers could not be vouched for) is superseded in place; the
    /// attention item filed at retirement keeps that history.
    pub(crate) fn record_execution_bookmark(&self, record: &ExecutionBookmark) -> Result<()> {
        let changed = self.connect()?.execute(
            "INSERT INTO execution_bookmarks (execution_id, repo_path, host_id, base_commit) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(execution_id) DO UPDATE SET
                repo_path = excluded.repo_path, host_id = excluded.host_id, base_commit = excluded.base_commit,
                recovered_from = NULL, recovered_work = NULL, reconciliation_note = NULL,
                retired_at = NULL, retired_reason = NULL
             WHERE retired_at IS NOT NULL",
            params![
                record.execution_id,
                record.repo_path.to_str().context("shared repo path is not UTF-8")?,
                record.host_id,
                record.base_commit,
            ],
        )?;
        anyhow::ensure!(changed == 1, "execution bookmark record already exists and is live");
        Ok(())
    }

    pub(crate) fn update_execution_bookmark_base(&self, execution_id: &str, commit: &str) -> Result<()> {
        let changed = self.connect()?.execute(
            "UPDATE execution_bookmarks SET base_commit = ?2 WHERE execution_id = ?1 AND retired_at IS NULL",
            params![execution_id, commit],
        )?;
        anyhow::ensure!(changed == 1, "live execution bookmark record missing");
        Ok(())
    }

    /// Persist the note dispatch wrote when it re-anchored this execution's
    /// recovery source, for the worker prompt.
    pub(crate) fn record_bookmark_reconciliation_note(&self, execution_id: &str, note: &str) -> Result<()> {
        let changed = self.connect()?.execute(
            "UPDATE execution_bookmarks SET reconciliation_note = ?2 WHERE execution_id = ?1",
            params![execution_id, note],
        )?;
        anyhow::ensure!(changed == 1, "reconciliation execution bookmark record missing");
        Ok(())
    }

    pub(crate) fn bookmark_reconciliation_note(&self, execution_id: &str) -> Result<Option<String>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT reconciliation_note FROM execution_bookmarks WHERE execution_id = ?1",
                [execution_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Mark a record as retired: its pointers are no longer a recovery
    /// source (reads treat it as absent) and the abandoned-bookmark sweep
    /// leaves it alone. The row stays so the decision is auditable.
    pub(crate) fn retire_execution_bookmark(&self, execution_id: &str, reason: &str) -> Result<()> {
        let changed = self.connect()?.execute(
            "UPDATE execution_bookmarks SET retired_at = ?2, retired_reason = ?3 WHERE execution_id = ?1 AND retired_at IS NULL",
            params![execution_id, now_string(), reason],
        )?;
        anyhow::ensure!(
            changed == 1,
            "execution bookmark record to retire is missing or already retired"
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn execution_bookmark_retired_reason(&self, execution_id: &str) -> Result<Option<String>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT retired_reason FROM execution_bookmarks WHERE execution_id = ?1 AND retired_at IS NOT NULL",
                [execution_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub(crate) fn delete_execution_restore_report(&self, execution_id: &str) -> Result<()> {
        self.connect()?.execute(
            "DELETE FROM execution_restore_reports WHERE execution_id = ?1",
            [execution_id],
        )?;
        Ok(())
    }

    /// Drop the bookmark row and any restore report in one transaction, so a
    /// run degraded to clean dispatch cannot be treated as already recovered.
    pub(crate) fn delete_execution_bookmark_and_restore_report(&self, execution_id: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM execution_bookmarks WHERE execution_id = ?1",
            [execution_id],
        )?;
        tx.execute(
            "DELETE FROM execution_restore_reports WHERE execution_id = ?1",
            [execution_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn execution_bookmark(&self, execution_id: &str) -> Result<ExecutionBookmark> {
        self.execution_bookmark_optional(execution_id)?
            .with_context(|| format!("no engine-created recovery bookmark recorded for execution {execution_id}"))
    }

    pub(crate) fn execution_bookmark_optional(&self, execution_id: &str) -> Result<Option<ExecutionBookmark>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT execution_id, repo_path, host_id, base_commit FROM execution_bookmarks
                 WHERE execution_id = ?1 AND retired_at IS NULL",
                [execution_id],
                |row| {
                    Ok(ExecutionBookmark {
                        execution_id: row.get(0)?,
                        repo_path: std::path::PathBuf::from(row.get::<_, String>(1)?),
                        host_id: row.get(2)?,
                        base_commit: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Resolve the predecessor by recorded execution identity alone. Workspace
    /// preferences, lease ownership, and filesystem existence are irrelevant.
    pub(crate) fn recovery_predecessor(&self, execution: &WorkExecution) -> Result<Option<WorkExecution>> {
        let conn = self.connect()?;
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM work_executions WHERE work_item_id = ?1 AND id != ?2
             AND kind = ?3 AND (started_at IS NOT NULL OR EXISTS (
                 SELECT 1 FROM execution_bookmarks WHERE execution_id = work_executions.id
             )) AND created_at <= ?4 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                params![
                    execution.work_item_id,
                    execution.id,
                    execution.kind.as_str(),
                    execution.created_at
                ],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = id else { return Ok(None) };
        let prior = query_execution(&conn, &id)?.context("recovery predecessor disappeared")?;
        Ok(prior.status.is_terminal().then_some(prior))
    }
}
