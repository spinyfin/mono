//! Which shared jj stores still need their one-shot legacy bookmark cleanup.
//!
//! Earlier engines left `boss-base/*`, `boss-recovery/*` and `boss/*`
//! bookmarks (and cube a `pr/<n>` alias) in the shared store of every
//! repository they dispatched into. The migration records each store the engine
//! has ever used, taken from `execution_bookmarks`, as pending. The runner
//! (`boss_engine::legacy_store_cleanup`) cleans one store at a time and marks
//! it completed, so a store is cleaned once and never again, a store first seen
//! after the upgrade is never listed (only publication branches are still created), and
//! a failure leaves the store pending for a later attempt.
use super::*;
use boss_engine_recovery::execution_bookmark::LegacyCleanupReport;

pub(crate) const MIGRATION_ID: &str = "20261110_0755_legacy_store_cleanup";

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS legacy_store_cleanups (
            host_id TEXT NOT NULL,
            repo_path TEXT NOT NULL,
            cube_repo_id TEXT,
            attempts INTEGER NOT NULL DEFAULT 0,
            last_attempt_at TEXT,
            last_error TEXT,
            completed_at TEXT,
            archived_count INTEGER NOT NULL DEFAULT 0,
            forgotten_count INTEGER NOT NULL DEFAULT 0,
            kept_count INTEGER NOT NULL DEFAULT 0,
            gc_error TEXT,
            PRIMARY KEY (host_id, repo_path)
         );
         INSERT OR IGNORE INTO legacy_store_cleanups (host_id, repo_path, cube_repo_id)
            SELECT eb.host_id, eb.repo_path, MAX(we.cube_repo_id)
            FROM execution_bookmarks eb
            LEFT JOIN work_executions we ON we.id = eb.execution_id
            GROUP BY eb.host_id, eb.repo_path;",
    )?;
    Ok(())
}

/// A store whose cleanup has not completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyStoreCleanup {
    pub host_id: String,
    pub repo_path: PathBuf,
    /// The cube repo id for `cube repo gc`, when an execution recorded it.
    pub cube_repo_id: Option<String>,
    pub attempts: i64,
}

/// How a store's cleanup stands, for tests and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct LegacyStoreCleanupStatus {
    pub completed: bool,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub archived: i64,
    pub forgotten: i64,
    pub kept: i64,
    pub gc_error: Option<String>,
}

fn is_pushed_head(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Stores still to clean whose last attempt, if any, was at least
    /// `retry_after_secs` ago.
    pub fn pending_legacy_store_cleanups(&self, retry_after_secs: i64) -> Result<Vec<LegacyStoreCleanup>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT host_id, repo_path, cube_repo_id, attempts FROM legacy_store_cleanups
             WHERE completed_at IS NULL
               AND (last_attempt_at IS NULL OR CAST(last_attempt_at AS INTEGER) <= unixepoch('now') - ?1)
             ORDER BY host_id, repo_path",
        )?;
        let rows = stmt
            .query_map([retry_after_secs], |row| {
                Ok(LegacyStoreCleanup {
                    host_id: row.get(0)?,
                    repo_path: PathBuf::from(row.get::<_, String>(1)?),
                    cube_repo_id: row.get(2)?,
                    attempts: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every commit id the engine recorded as pushed to a PR branch: the heads
    /// executions started from and finished on, the heads the merge poller
    /// observed, and the heads conflict resolution pushed. Everything reachable
    /// from one of them is on the remote, whatever has happened to the branch
    /// since, so none of it is unpublished work.
    pub fn pushed_pr_heads(&self) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT head FROM (
                SELECT pr_head_before AS head FROM work_executions
                UNION SELECT pr_head_after FROM work_executions
                UNION SELECT pr_head_sha FROM tasks
                UNION SELECT head_sha_before FROM conflict_resolutions
                UNION SELECT head_sha_after FROM conflict_resolutions
             ) WHERE head IS NOT NULL AND length(head) = 40",
        )?;
        let heads = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);
        let mut heads: Vec<String> = heads
            .into_iter()
            .map(|head| head.to_ascii_lowercase())
            .filter(|head| is_pushed_head(head))
            .collect();
        // Restaging duplicates across change identities. A recorded later push
        // proves the preserved source was carried; reuse the cleanup lineage.
        for cleanup in self.list_superseded_preserved(i32::MAX as usize, &[])? {
            if let Some(tip) = self.preserved_tip(&cleanup.execution.id)? {
                heads.push(tip.commit_id);
            }
        }
        heads.sort();
        heads.dedup();
        Ok(heads)
    }

    /// Count an attempt on `host_id`'s store at `repo_path`.
    pub fn begin_legacy_store_cleanup(&self, host_id: &str, repo_path: &Path) -> Result<()> {
        self.connect()?.execute(
            "UPDATE legacy_store_cleanups SET attempts = attempts + 1, last_attempt_at = ?3
             WHERE host_id = ?1 AND repo_path = ?2 AND completed_at IS NULL",
            params![host_id, path_text(repo_path)?, now_string()],
        )?;
        Ok(())
    }

    /// Keep the store pending and remember why the attempt failed.
    pub fn fail_legacy_store_cleanup(&self, host_id: &str, repo_path: &Path, error: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE legacy_store_cleanups SET last_error = ?3
             WHERE host_id = ?1 AND repo_path = ?2 AND completed_at IS NULL",
            params![host_id, path_text(repo_path)?, error],
        )?;
        let kind = boss_engine_storage_values::attention_kinds::LEGACY_STORE_CLEANUP_FAILED_ATTENTION_KIND;
        // Every migrated store comes from an execution recovery record. Retain
        // dismissed signals so retries never nag again, even after a restart.
        let owner: Option<String> = tx
            .query_row(
                "SELECT eb.execution_id FROM execution_bookmarks eb
             JOIN legacy_store_cleanups lc ON lc.host_id = eb.host_id AND lc.repo_path = eb.repo_path
             JOIN work_executions we ON we.id = eb.execution_id
             WHERE lc.host_id = ?1 AND lc.repo_path = ?2 AND lc.attempts >= 3
               AND lc.completed_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM work_attention_items ai
                   JOIN execution_bookmarks owner ON owner.execution_id = ai.execution_id
                   WHERE owner.host_id = lc.host_id AND owner.repo_path = lc.repo_path AND ai.kind = ?3)
             ORDER BY eb.execution_id LIMIT 1",
                params![host_id, path_text(repo_path)?, kind],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(owner) = owner {
            super::attention_filing::upsert_execution_attention_in_tx(
                &tx,
                &owner,
                kind,
                "Legacy store cleanup keeps failing",
                &format!(
                    "Cleanup of `{}` on host `{host_id}` has failed at least three times. Automatic retries continue hourly; archive cleanup for this store remains suspended until cleanup completes.\n\nLast error:\n\n{error}",
                    repo_path.display()
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A removed host cannot be retried. Retain the reason in diagnostics.
    pub fn retire_legacy_store_cleanup(&self, host_id: &str, repo_path: &Path, reason: &str) -> Result<()> {
        self.connect()?.execute(
            "UPDATE legacy_store_cleanups SET completed_at = ?3, last_error = ?4
             WHERE host_id = ?1 AND repo_path = ?2 AND completed_at IS NULL",
            params![host_id, path_text(repo_path)?, now_string(), reason],
        )?;
        Ok(())
    }

    /// Mark the store cleaned, and let the closed-chain cleanup forget the
    /// archives made for work found in it once the item is long closed.
    pub fn finish_legacy_store_cleanup(
        &self,
        host_id: &str,
        repo_path: &Path,
        report: &LegacyCleanupReport,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE legacy_store_cleanups
             SET completed_at = ?3, last_error = NULL,
                 archived_count = ?4, forgotten_count = ?5, kept_count = ?6
             WHERE host_id = ?1 AND repo_path = ?2",
            params![
                host_id,
                path_text(repo_path)?,
                now_string(),
                report.archived.len() as i64,
                report.forgotten as i64,
                report.kept.len() as i64
            ],
        )?;
        let executions: std::collections::BTreeSet<_> = report.archived.iter().map(|a| &a.execution_id).collect();
        for execution in executions {
            // Some legacy names predate the database's execution history. Keep
            // their archives indefinitely rather than inventing an owner.
            if query_execution(&tx, execution)?.is_none() {
                continue;
            }
            let names = report
                .archived
                .iter()
                .filter(|a| &a.execution_id == execution)
                .map(|a| format!("- `{}` (from `{}`)", a.archive, a.source))
                .collect::<Vec<_>>()
                .join("\n");
            let kind = boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND;
            let existing: Option<String> = tx.query_row(
                "SELECT body_markdown FROM work_attention_items WHERE execution_id = ?1 AND kind = ?2 AND status != 'resolved' ORDER BY created_at, id LIMIT 1",
                params![execution, kind], |row| row.get(0),
            ).optional()?;
            let note = format!(
                "Legacy store cleanup found unpublished work in `{}` on `{host_id}`. Inspect these archives before dismissing this item:\n\n{names}",
                repo_path.display()
            );
            let body = match existing {
                Some(body) if body.contains(&note) => body,
                Some(body) => format!("{body}\n\n{note}"),
                None => note,
            };
            super::attention_filing::upsert_execution_attention_in_tx(
                &tx,
                execution,
                kind,
                "Unpublished work recovered from legacy bookmarks",
                &body,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Record how the follow-up `cube repo gc` ended. It is maintenance, not
    /// part of the cleanup: cube also collects once a day.
    pub fn record_legacy_store_gc(&self, host_id: &str, repo_path: &Path, error: Option<&str>) -> Result<()> {
        self.connect()?.execute(
            "UPDATE legacy_store_cleanups SET gc_error = ?3 WHERE host_id = ?1 AND repo_path = ?2",
            params![host_id, path_text(repo_path)?, error],
        )?;
        Ok(())
    }

    pub fn legacy_store_cleanup_status(
        &self,
        host_id: &str,
        repo_path: &Path,
    ) -> Result<Option<LegacyStoreCleanupStatus>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT completed_at IS NOT NULL, attempts, last_error, archived_count, forgotten_count, kept_count,
                        gc_error
                 FROM legacy_store_cleanups WHERE host_id = ?1 AND repo_path = ?2",
                params![host_id, path_text(repo_path)?],
                |row| {
                    Ok(LegacyStoreCleanupStatus {
                        completed: row.get(0)?,
                        attempts: row.get(1)?,
                        last_error: row.get(2)?,
                        archived: row.get(3)?,
                        forgotten: row.get(4)?,
                        kept: row.get(5)?,
                        gc_error: row.get(6)?,
                    })
                },
            )
            .optional()?)
    }
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("shared repo path is not UTF-8")
}
