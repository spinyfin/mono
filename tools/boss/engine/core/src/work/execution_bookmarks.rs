use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use boss_engine_recovery::execution_bookmark::{ExecutionBookmark, Jj, LocalJj, PointerSnapshot};

/// [`LocalJj`] with a child-process count, so a pointer snapshot / restore
/// pass can publish how many `jj` processes it spawned
/// (`execution_bookmark_pointers.jj_spawns`). Spawns are the whole cost of
/// these passes; the count is what makes a regression visible.
struct CountingJj {
    spawns: AtomicU64,
}

impl CountingJj {
    fn new() -> Self {
        Self {
            spawns: AtomicU64::new(0),
        }
    }

    fn spawns(&self) -> u64 {
        self.spawns.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl Jj for CountingJj {
    async fn run(&self, repo: &std::path::Path, args: &[&str]) -> Result<String> {
        self.spawns.fetch_add(1, Ordering::Relaxed);
        LocalJj.run(repo, args).await
    }

    async fn shared_repo(&self, workspace: &std::path::Path) -> Result<std::path::PathBuf> {
        LocalJj.shared_repo(workspace).await
    }
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Live local recovery records of the PR-owning chain: the root item and
    /// every revision share the PR branch in the shared store, so an engine
    /// rebase moves all of their pointers. Executions with a live worker are
    /// left out: the rebase also rewrites that worker's working copy, and jj's
    /// own rewrite-following keeps its bookmarks consistent with it. Restoring
    /// them to pre-rebase commits would strand them on a divergent twin.
    fn chain_bookmark_records(&self, chain_root_id: &str) -> Vec<ExecutionBookmark> {
        let executions = match self.list_executions_for_chain(chain_root_id) {
            Ok(executions) => executions,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    chain_root_id,
                    "cannot list chain executions for recovery pointers"
                );
                return Vec::new();
            }
        };
        let mut seen = std::collections::HashSet::new();
        let mut records = Vec::new();
        for execution in executions {
            if !seen.insert(execution.id.clone()) || execution.status.is_live() {
                continue;
            }
            match self.execution_bookmark_optional(&execution.id) {
                Ok(Some(record)) if record.host_id == "local" => records.push(record),
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(execution_id = %execution.id, ?err, "cannot read recovery bookmark record");
                }
            }
        }
        records
    }

    /// Preserve recovery identities across cube's shared-store pointer
    /// rewrites. Best-effort per record: one damaged historical record is
    /// logged and left out so it cannot disable the rebase for the others.
    pub(crate) async fn snapshot_rebase_pointers(
        &self,
        chain_root_id: &str,
    ) -> Vec<(ExecutionBookmark, PointerSnapshot)> {
        use boss_engine_recovery::execution_bookmark::snapshot_pointers;
        let registry = self.metrics_registry();
        let timer = crate::sweep_metrics::EXECUTION_BOOKMARK_POINTERS.begin();
        let jj = CountingJj::new();
        let records = self.chain_bookmark_records(chain_root_id);
        let items = records.len();
        let mut saved = Vec::new();
        for record in records {
            match snapshot_pointers(&jj, &record).await {
                Ok(pointers) => saved.push((record, pointers)),
                Err(err) => {
                    tracing::warn!(execution_id = %record.execution_id, ?err, "cannot snapshot recovery pointers; leaving this record unprotected");
                }
            }
        }
        crate::sweep_metrics::EXECUTION_BOOKMARK_POINTERS_JJ_SPAWNS.inc_by(registry, jj.spawns());
        timer.finish(registry, items);
        saved
    }

    /// Attempt every restore even if one repository is unavailable. A published
    /// rebase must still pass the deletion gate when maintenance fails.
    pub(crate) async fn restore_rebase_pointers(&self, saved: &[(ExecutionBookmark, PointerSnapshot)]) -> bool {
        use boss_engine_recovery::execution_bookmark::restore_pointers;
        let registry = self.metrics_registry();
        let timer = crate::sweep_metrics::EXECUTION_BOOKMARK_POINTERS.begin();
        let jj = CountingJj::new();
        let mut restored = true;
        for (record, pointers) in saved {
            if let Err(err) = restore_pointers(&jj, record, pointers).await {
                restored = false;
                tracing::error!(execution_id = %record.execution_id, ?err, "could not restore recovery pointers after cube rebase");
            }
        }
        crate::sweep_metrics::EXECUTION_BOOKMARK_POINTERS_JJ_SPAWNS.inc_by(registry, jj.spawns());
        timer.finish(registry, saved.len());
        restored
    }

    /// End of a mechanical rebase operation: put every saved pointer back, then,
    /// when the operation pushed a new PR head (`pushed_branch`), move each
    /// record whose preserved work that head already carries onto it as one set
    /// and persist the baseline. Records with unpublished work keep their saved
    /// identities. Never fails the caller; maintenance problems are logged.
    pub(crate) async fn finish_rebase_pointers(
        &self,
        saved: &[(ExecutionBookmark, PointerSnapshot)],
        pushed_branch: Option<&str>,
        trunk: &str,
    ) -> bool {
        use boss_engine_recovery::execution_bookmark::{adopt_pushed_head, resolve_bookmark_commit};
        // The branch moved with the rebase, so read it before restoring.
        let mut pushed = Vec::new();
        if let Some(branch) = pushed_branch {
            for (record, _) in saved {
                match resolve_bookmark_commit(&LocalJj, record, branch).await {
                    Ok(Some(commit)) => pushed.push((record, commit)),
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(execution_id = %record.execution_id, ?err, "cannot resolve pushed PR head; keeping saved pointers");
                    }
                }
            }
        }
        let restored = self.restore_rebase_pointers(saved).await;
        if !restored {
            return false;
        }
        for (record, commit) in pushed {
            let result: Result<()> = async {
                adopt_pushed_head(&LocalJj, record, &commit, trunk).await?;
                self.update_execution_bookmark_base(&record.execution_id, &commit)
            }
            .await;
            if let Err(err) = result {
                tracing::warn!(execution_id = %record.execution_id, ?err, "recovery pointers not moved onto the pushed PR head; keeping saved pointers");
            }
        }
        true
    }

    /// Checkpoint intact records independently without changing ladder outcomes.
    pub(crate) async fn checkpoint_rewritten_baselines(&self, chain_root_id: &str) -> Result<()> {
        use boss_engine_recovery::execution_bookmark::checkpoint_baseline;
        for record in self.chain_bookmark_records(chain_root_id) {
            let result: Result<()> = async {
                let base = checkpoint_baseline(&LocalJj, &record).await?;
                self.update_execution_bookmark_base(&record.execution_id, &base)
            }
            .await;
            if let Err(err) = result {
                tracing::warn!(execution_id = %record.execution_id, ?err, "rebase baseline checkpoint failed; keeping recorded baseline");
            }
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
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
