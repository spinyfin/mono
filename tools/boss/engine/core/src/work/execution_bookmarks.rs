use super::*;
use boss_engine_recovery::execution_bookmark::ExecutionBookmark;

pub(super) fn migrate_execution_bookmarks(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS execution_bookmarks (
        execution_id TEXT PRIMARY KEY REFERENCES work_executions(id),
        repo_path TEXT NOT NULL,
        host_id TEXT NOT NULL,
        recovered_from TEXT,
        recovered_work INTEGER
    );",
    )?;
    Ok(())
}

impl WorkDb {
    pub(crate) fn terminal_bookmark_executions(&self, grace: i64, lookback: i64) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM work_executions WHERE started_at IS NOT NULL
            AND status IN ('completed', 'failed', 'cancelled', 'orphaned', 'abandoned')
            AND CAST(finished_at AS INTEGER) <= unixepoch('now') - ?1
            AND CAST(finished_at AS INTEGER) >= unixepoch('now') - ?2",
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
    pub(crate) fn record_execution_bookmark(&self, record: &ExecutionBookmark) -> Result<()> {
        self.connect()?.execute(
            "INSERT INTO execution_bookmarks (execution_id, repo_path, host_id) VALUES (?1, ?2, ?3)",
            params![
                record.execution_id,
                record.repo_path.to_str().context("shared repo path is not UTF-8")?,
                record.host_id
            ],
        )?;
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
                "SELECT execution_id, repo_path, host_id FROM execution_bookmarks WHERE execution_id = ?1",
                [execution_id],
                |row| {
                    Ok(ExecutionBookmark {
                        execution_id: row.get(0)?,
                        repo_path: std::path::PathBuf::from(row.get::<_, String>(1)?),
                        host_id: row.get(2)?,
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
             AND started_at IS NOT NULL ORDER BY created_at DESC, id DESC LIMIT 1",
                params![execution.work_item_id, execution.id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = id else { return Ok(None) };
        let prior = query_execution(&conn, &id)?.context("recovery predecessor disappeared")?;
        Ok(prior.status.is_terminal().then_some(prior))
    }
}
