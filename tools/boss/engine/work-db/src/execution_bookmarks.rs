use super::*;
use crate::schema_init::table_has_column;
use boss_engine_recovery::execution_bookmark::ExecutionBookmark;
use rusqlite::Connection;

pub(crate) const HAS_RECOVERY_HEAD_MIGRATION_ID: &str = "20261107_0815_execution_bookmark_has_recovery_head";

pub(crate) fn migrate_has_recovery_head(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "execution_bookmarks", "has_recovery_head")? {
        conn.execute_batch("ALTER TABLE execution_bookmarks ADD COLUMN has_recovery_head INTEGER")?;
    }
    Ok(())
}

/// The prune sweep and the `boss-recovery/` head are gone, so the state they
/// kept on the record is too. Open `execution_bookmark_prune_unproven` items
/// described a sweep that no longer runs and nothing would ever resolve them.
pub(crate) const RETIRE_PRUNE_STATE_MIGRATION_ID: &str = "20261109_1500_execution_bookmark_retire_prune_state";

pub(crate) fn migrate_retire_prune_state(conn: &Connection) -> Result<()> {
    for column in ["prune_checked_at", "prune_outcome", "has_recovery_head"] {
        if table_has_column(conn, "execution_bookmarks", column)? {
            conn.execute_batch(&format!("ALTER TABLE execution_bookmarks DROP COLUMN {column}"))?;
        }
    }
    conn.execute(
        "UPDATE work_attention_items SET status = 'resolved', resolved_at = CAST(unixepoch('now') AS TEXT)
         WHERE kind = 'execution_bookmark_prune_unproven' AND status = 'open'",
        [],
    )?;
    Ok(())
}

#[work_db_boundary::instrument]
impl WorkDb {
    pub fn record_execution_restore_report(
        &self,
        execution_id: &str,
        report: &boss_engine_recovery::execution_bookmark::RestoreReport,
    ) -> Result<()> {
        self.connect()?.execute("INSERT INTO execution_restore_reports (execution_id, report) VALUES (?1, ?2) ON CONFLICT(execution_id) DO UPDATE SET report = excluded.report", params![execution_id, serde_json::to_string(report)?])?;
        Ok(())
    }

    pub fn execution_restore_report(
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

    pub fn terminal_bookmark_executions(&self, grace: i64, lookback: i64) -> Result<Vec<WorkExecution>> {
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
    pub fn record_bookmark_recovery(&self, execution_id: &str, predecessor: &str, has_work: bool) -> Result<()> {
        let changed = self.connect()?.execute(
            "UPDATE execution_bookmarks SET recovered_from = ?2, recovered_work = ?3 WHERE execution_id = ?1",
            params![execution_id, predecessor, has_work],
        )?;
        anyhow::ensure!(changed == 1, "recovery execution bookmark record missing");
        Ok(())
    }

    pub fn bookmark_recovery(&self, execution_id: &str) -> Result<Option<(String, bool)>> {
        Ok(self.connect()?.query_row("SELECT recovered_from, recovered_work FROM execution_bookmarks WHERE execution_id = ?1 AND recovered_from IS NOT NULL", [execution_id], |row| Ok((row.get(0)?, row.get(1)?))).optional()?)
    }
    /// A retired row for the same execution (a deliberate fresh start after
    /// its own pointers could not be vouched for) is superseded in place; the
    /// attention item filed at retirement keeps that history.
    pub fn record_execution_bookmark(&self, record: &ExecutionBookmark) -> Result<()> {
        let changed = self.connect()?.execute(
            "INSERT INTO execution_bookmarks (execution_id, repo_path, host_id, base_commit)
             VALUES (?1, ?2, ?3, ?4)
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

    /// Mark a record as retired: its pointers are no longer a recovery
    /// source (reads treat it as absent) and the abandoned-bookmark sweep
    /// leaves it alone. The row stays so the decision is auditable.
    pub fn retire_execution_bookmark(&self, execution_id: &str, reason: &str) -> Result<()> {
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

    #[cfg(any(test, feature = "test-support"))]
    pub fn execution_bookmark_retired_reason(&self, execution_id: &str) -> Result<Option<String>> {
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

    pub fn delete_execution_restore_report(&self, execution_id: &str) -> Result<()> {
        self.connect()?.execute(
            "DELETE FROM execution_restore_reports WHERE execution_id = ?1",
            [execution_id],
        )?;
        Ok(())
    }

    /// Drop the bookmark row and any restore report in one transaction, so a
    /// run degraded to clean dispatch cannot be treated as already recovered.
    pub fn delete_execution_bookmark_and_restore_report(&self, execution_id: &str) -> Result<()> {
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

    pub fn execution_bookmark(&self, execution_id: &str) -> Result<ExecutionBookmark> {
        self.execution_bookmark_optional(execution_id)?
            .with_context(|| format!("no engine-created recovery bookmark recorded for execution {execution_id}"))
    }

    pub fn execution_bookmark_optional(&self, execution_id: &str) -> Result<Option<ExecutionBookmark>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT eb.execution_id, eb.repo_path, eb.host_id, eb.base_commit, we.pr_head_before
                 FROM execution_bookmarks eb
                 LEFT JOIN work_executions we ON we.id = eb.execution_id
                 WHERE eb.execution_id = ?1 AND eb.retired_at IS NULL",
                [execution_id],
                |row| {
                    Ok(ExecutionBookmark {
                        execution_id: row.get(0)?,
                        repo_path: std::path::PathBuf::from(row.get::<_, String>(1)?),
                        host_id: row.get(2)?,
                        base_commit: row.get(3)?,
                        pr_head_before: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// The execution whose recovery state a dispatch of `execution` consumes:
    /// `execution` itself when it already has a preserved tip or a live
    /// record (a retry), otherwise its recovery predecessor. Both dispatch
    /// recovery and host placement start from this one lookup.
    ///
    /// Kinds that [`boss_protocol::ExecutionKind::never_writes_pr_branch`]
    /// have no recovery source: they inherit neither a predecessor's record
    /// nor their own, matching [`Self::recovery_predecessor`].
    pub fn recovery_source(&self, execution: &WorkExecution) -> Result<Option<RecoverySource>> {
        if execution.kind.never_writes_pr_branch() {
            return Ok(None);
        }
        let own_tip = self.preserved_tip(&execution.id)?.is_some();
        let source = if own_tip || self.execution_bookmark_optional(&execution.id)?.is_some() {
            execution.clone()
        } else if let Some(prior) = self.recovery_predecessor(execution)? {
            prior
        } else {
            return Ok(None);
        };
        let has_preserved_tip = own_tip || self.preserved_tip(&source.id)?.is_some();
        Ok(Some(RecoverySource {
            execution: source,
            has_preserved_tip,
        }))
    }

    /// The recovery predecessor whose preserved work a dispatch of `execution`
    /// would restore, when that predecessor still holds its cube workspace
    /// lease. Its unpublished work is preserved only as its lease is released
    /// (preservation precedes the release, and the release clears the lease
    /// columns), so while the lease is recorded the dispatch cannot tell "no
    /// work to restore" from "work not yet preserved". Dispatch holds the row
    /// instead of guessing, and starting it from trunk would strand whatever
    /// the predecessor left unpublished.
    ///
    /// `None` when `execution` recovers from its own record (a retry) or has no
    /// predecessor, or when the predecessor's lease is gone. Lease age does not
    /// prove that unpublished work has been preserved.
    pub fn recovery_predecessor_holding_lease(&self, execution: &WorkExecution) -> Result<Option<WorkExecution>> {
        let Some(source) = self.recovery_source(execution)? else {
            return Ok(None);
        };
        let predecessor = source.execution;
        if predecessor.id == execution.id {
            return Ok(None);
        }
        let holds_lease = predecessor
            .cube_lease_id
            .as_deref()
            .is_some_and(|lease| !lease.is_empty());
        Ok(holds_lease.then_some(predecessor))
    }

    /// Resolve the predecessor by recorded execution identity alone. Workspace
    /// preferences, lease ownership, and filesystem existence are irrelevant.
    ///
    /// A read-only review kind ([`boss_protocol::ExecutionKind::never_writes_pr_branch`]) never
    /// has a recovery predecessor. A reviewer never writes to the PR branch,
    /// so an earlier reviewer of the same item has no unpublished work for it
    /// to inherit; the only effects inheriting that record could have are to
    /// pin the review to the record's host, to reposition it on the head the
    /// earlier reviewer looked at instead of the PR's current head, and to
    /// fail it before start whenever that record's pointers are damaged, as
    /// happens when the PR's history is rewritten after a review (mono#3201,
    /// sixteen reviews dead in pre-start recovery with the head unreviewed).
    /// Its own record, when a self-retry has one, is still consulted by
    /// dispatch; only inheritance is ruled out here.
    pub fn recovery_predecessor(&self, execution: &WorkExecution) -> Result<Option<WorkExecution>> {
        if execution.kind.never_writes_pr_branch() {
            return Ok(None);
        }
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

/// The execution a dispatch recovers from, and whether it carries a recorded
/// preserved tip (restorable unpublished work).
#[derive(Debug, Clone)]
pub struct RecoverySource {
    pub execution: WorkExecution,
    pub has_preserved_tip: bool,
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// File a preserved-work inspection verdict for `execution_id`, or fold it
    /// into the open item: earlier evidence is kept and only commits and a
    /// reason the body does not yet carry are appended.
    pub fn raise_preserved_work_inspect_attention(
        &self,
        execution_id: &str,
        inspect: &boss_engine_recovery::execution_bookmark::PreserveInspect,
    ) -> Result<String> {
        let kind = boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND;
        let title = "Preserved work needs inspection";
        let open = self
            .list_attention_items(execution_id)?
            .into_iter()
            .find(|item| item.kind == kind && item.status == "open");
        let Some(existing) = open else {
            return self.upsert_execution_attention(execution_id, kind, title, &inspect_body(inspect));
        };
        let mut body = existing.body_markdown;
        if let Some(commit) = inspect.existing_commit_id.as_deref()
            && !body.contains(&format!("`{commit}`"))
        {
            body.push_str(&format!("\n- existing preserved commit: `{commit}`"));
        }
        if let Some(line) = inspect_workspace_line(inspect)
            && inspect
                .workspace_commit_id
                .as_deref()
                .is_none_or(|commit| !body.contains(&format!("commit `{commit}`")))
        {
            body.push('\n');
            body.push_str(&line);
        }
        if !inspect.reason.is_empty() && !body.contains(&inspect.reason) {
            body.push_str(&format!("\n- {}", inspect.reason));
        }
        self.upsert_execution_attention(execution_id, kind, title, &body)
    }

    /// A resolved informational note: archiving is successful recovery handling,
    /// never a request for intervention or a reason to disable autostart.
    pub fn record_recovery_fresh_start(
        &self,
        prior: &WorkExecution,
        next_id: &str,
        reason: &str,
        archives: &[(String, String)],
    ) -> Result<()> {
        let preserved_note = self.preserved_tip(&prior.id)?.map(|tip| format!(
            "Preserved work: `preserved/{}` retains `{}`. This is separate from the legacy archive bookmarks above.", prior.id, tip.commit_id
        )).unwrap_or_default();
        self.record_recovery_archive_note(
            prior,
            reason,
            archives,
            (
                "Recovery source retired; starting fresh",
                &if archives.is_empty() {
                    format!(
                        "Execution `{next_id}` starts fresh from an open PR, trunk, or the captured immutable review target. {preserved_note} No action is required."
                    )
                } else {
                    format!(
                        "Execution `{next_id}` starts fresh from an open PR, trunk, or the captured immutable review target. {preserved_note} Legacy pointer targets remain at the archive bookmarks listed above; inspect the preserved live tip and archives if that work is still wanted."
                    )
                },
            ),
        )
    }

    fn record_recovery_archive_note(
        &self,
        prior: &WorkExecution,
        reason: &str,
        archives: &[(String, String)],
        wording: (&str, &str),
    ) -> Result<()> {
        use boss_engine_storage_values::attention_kinds::EXECUTION_BOOKMARK_RECOVERY_RETIRED_ATTENTION_KIND;
        let mut retained = archives
            .iter()
            .map(|(name, commit)| format!("`{name}` retains `{commit}`"))
            .collect::<Vec<_>>()
            .join("\n");
        if retained.is_empty() {
            retained = "No archive bookmarks were created.".to_owned();
        }
        self.create_attention_item(CreateAttentionItemInput {
            execution_id: None,
            work_item_id: Some(self.owning_work_item_id(&prior.work_item_id)),
            kind: EXECUTION_BOOKMARK_RECOVERY_RETIRED_ATTENTION_KIND.to_owned(),
            status: Some("resolved".to_owned()),
            title: wording.0.to_owned(),
            body_markdown: format!(
                "Execution `{}` encountered a recovery problem: {reason}\n\nRecovery archive:\n{retained}\n\n{}",
                prior.id, wording.1
            ),
            resolved_at: Some(now_string()),
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod has_recovery_head_migration_tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn upgrade_leaves_existing_rows_unknown() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE execution_bookmarks (
                execution_id TEXT PRIMARY KEY,
                repo_path TEXT NOT NULL,
                host_id TEXT NOT NULL
            );
            INSERT INTO execution_bookmarks VALUES ('old', '/repo', 'local');",
        )
        .unwrap();
        migrate_has_recovery_head(&conn).unwrap();
        migrate_has_recovery_head(&conn).unwrap();
        let flag: Option<i64> = conn
            .query_row(
                "SELECT has_recovery_head FROM execution_bookmarks WHERE execution_id = 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(flag, None);
    }
}

#[cfg(test)]
mod pr_head_before_loader_tests {
    use super::*;
    use crate::test_support::{create_ready_chore_execution, create_test_chore_manual, create_test_product};
    use boss_engine_recovery::execution_bookmark::ExecutionBookmark;
    use std::path::PathBuf;

    #[test]
    fn loader_attaches_pr_head_before_from_the_execution_row() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id, "bound PR");
        let execution = create_ready_chore_execution(&db, chore.id);
        db.set_execution_pr_head_before(&execution.id, "abc123def456abc123def456abc123def456abc1")
            .unwrap();
        db.record_execution_bookmark(&ExecutionBookmark {
            execution_id: execution.id.clone(),
            repo_path: PathBuf::from("/tmp/shared"),
            host_id: "local".into(),
            base_commit: None,
            pr_head_before: None,
        })
        .unwrap();
        let record = db.execution_bookmark(&execution.id).unwrap();
        assert_eq!(record.base_commit, None);
        assert_eq!(
            record.pr_head_before.as_deref(),
            Some("abc123def456abc123def456abc123def456abc1")
        );
    }
}

fn inspect_workspace_line(inspect: &boss_engine_recovery::execution_bookmark::PreserveInspect) -> Option<String> {
    match (
        inspect.workspace_change_id.as_deref(),
        inspect.workspace_commit_id.as_deref(),
        inspect.workspace_count,
    ) {
        (Some(change), Some(commit), Some(count)) => Some(format!(
            "- workspace tip: change `{change}`, commit `{commit}`, {count} unpublished commits"
        )),
        _ => None,
    }
}

fn inspect_body(inspect: &boss_engine_recovery::execution_bookmark::PreserveInspect) -> String {
    let existing = inspect.existing_commit_id.as_deref().unwrap_or("none");
    let mut body = format!(
        "{}. Existing preserved commit: `{existing}`. Nothing was deleted.",
        inspect.reason
    );
    if let Some(line) = inspect_workspace_line(inspect) {
        body.push('\n');
        body.push_str(&line);
    } else {
        body.push_str(" New workspace tip: unavailable.");
    }
    body
}

#[cfg(test)]
mod inspect_attention_tests {
    use super::*;
    use crate::test_support::{create_ready_chore_execution, create_test_chore_manual, create_test_product};
    use boss_engine_recovery::execution_bookmark::PreserveInspect;
    use std::path::PathBuf;

    fn inspect(reason: &str) -> PreserveInspect {
        PreserveInspect {
            existing_commit_id: None,
            workspace_change_id: None,
            workspace_commit_id: None,
            workspace_count: None,
            reason: reason.into(),
        }
    }

    #[test]
    fn a_later_reason_is_appended_to_the_open_item_once() {
        let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id, "inspect");
        let execution = create_ready_chore_execution(&db, chore.id);
        let kind = boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND;

        db.raise_preserved_work_inspect_attention(&execution.id, &inspect("first reason"))
            .unwrap();
        db.raise_preserved_work_inspect_attention(&execution.id, &inspect("second reason"))
            .unwrap();
        db.raise_preserved_work_inspect_attention(&execution.id, &inspect("second reason"))
            .unwrap();

        let open: Vec<_> = db
            .list_attention_items(&execution.id)
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == kind && item.status == "open")
            .collect();
        assert_eq!(open.len(), 1, "one open item");
        let body = &open[0].body_markdown;
        assert!(body.contains("first reason"), "{body}");
        assert_eq!(body.matches("second reason").count(), 1, "{body}");
        assert!(body.ends_with("\n- second reason"), "{body}");
    }
}
