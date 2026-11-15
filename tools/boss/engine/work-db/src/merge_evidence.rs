//! Confirmed merge observations, independent of task completion.
use super::*;

pub(crate) const MIGRATION_ID: &str = "20261115_0110_merge_evidence";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pr_merge_evidence (
            root_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            canonical_pr_url TEXT NOT NULL,
            observed_at TEXT NOT NULL,
            PRIMARY KEY (root_task_id, canonical_pr_url)
         );",
    )?;
    install_journal_triggers(conn)
}

pub(super) fn install_journal_triggers(conn: &Connection) -> Result<()> {
    // Older migrations rebuild board triggers before this table exists.
    if !table_exists(conn, "pr_merge_evidence")? {
        return Ok(());
    }
    for (operation, rows) in [
        ("INSERT", "SELECT NEW.root_task_id"),
        ("UPDATE", "SELECT OLD.root_task_id UNION SELECT NEW.root_task_id"),
        ("DELETE", "SELECT OLD.root_task_id"),
    ] {
        conn.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS board_changes_pr_merge_evidence_{operation}
             AFTER {operation} ON pr_merge_evidence BEGIN
             INSERT INTO board_changes(item_id, source)
             SELECT root_task_id, 'pr_merge_evidence'
             FROM (WITH affected(root_task_id) AS ({rows}) SELECT root_task_id FROM affected);
             END;"
        ))?;
    }
    Ok(())
}

/// The first confirmed observation of this root's PR. This timestamp is
/// observation time, not GitHub's merge time (the existing caller omits it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeEvidence {
    pub root_task_id: String,
    pub canonical_pr_url: String,
    pub observed_at: String,
}

/// A merge observation can change evidence or queue state without completing
/// an already-terminal or human-driven task.
#[derive(Debug, Default)]
pub struct MergeObservation {
    pub completed_task: Option<Task>,
    pub changed: bool,
    /// Prior `merge_queue_state` when this observation actually cleared the
    /// Merging-lane columns. `None` when those columns were already empty.
    pub cleared_queue_state: Option<String>,
}

fn canonical_pr_url(pr_url: &str) -> Option<String> {
    let slug = boss_github::pr_url::repo_from_pr_url_lenient(pr_url)?;
    let number = boss_github::pr_url::pr_number_from_url(pr_url)?;
    Some(format!("https://github.com/{}/pull/{number}", slug.to_lowercase()))
}

pub(crate) fn same_binding(left: &str, right: &str) -> bool {
    left == right || canonical_pr_url(left).is_some_and(|url| Some(url) == canonical_pr_url(right))
}

fn current_binding(conn: &Connection, work_item_id: &str) -> Result<Option<(String, String)>> {
    let root = chain_helpers::chain_root(conn, work_item_id)?;
    let binding: Option<String> = conn
        .query_row(
            "SELECT pr_url FROM tasks WHERE id = ?1 AND deleted_at IS NULL AND kind != 'revision'",
            [&root],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(binding.and_then(|url| canonical_pr_url(&url).map(|url| (root, url))))
}

/// Read within the caller's database snapshot, with no GitHub access.
/// Rebinding hides evidence for the old PR; binding back to that same PR
/// restores its historical fact. A different PR never inherits it.
pub fn read_current(conn: &Connection, work_item_id: &str) -> Result<Option<MergeEvidence>> {
    let Some((root, url)) = current_binding(conn, work_item_id)? else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT root_task_id, canonical_pr_url, observed_at FROM pr_merge_evidence
             WHERE root_task_id = ?1 AND canonical_pr_url = ?2",
            params![root, url],
            |row| {
                Ok(MergeEvidence {
                    root_task_id: row.get(0)?,
                    canonical_pr_url: row.get(1)?,
                    observed_at: row.get(2)?,
                })
            },
        )
        .optional()?)
}

/// Only call after the shared PR lifecycle classification confirms a merge,
/// never on a merge request's success or a manual task completion.
pub(crate) fn record_in_tx(
    tx: &rusqlite::Transaction<'_>,
    work_item_id: &str,
    pr_url: &str,
    observed_at: &str,
) -> Result<bool> {
    let Some((root, bound_url)) = current_binding(tx, work_item_id)? else {
        return Ok(false);
    };
    if canonical_pr_url(pr_url).as_deref() != Some(bound_url.as_str()) {
        return Ok(false);
    }
    Ok(tx.execute(
        "INSERT INTO pr_merge_evidence(root_task_id, canonical_pr_url, observed_at)
         VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
        params![root, bound_url, observed_at],
    )? > 0)
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Evidence for the currently bound PR, including when queried via a revision.
    pub fn get_merge_evidence(&self, work_item_id: &str) -> Result<Option<MergeEvidence>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        read_current(&tx, work_item_id)
    }
}

/// Stage the root invalidation alongside newly recorded evidence.
pub(crate) fn record_and_stage(
    pending: &mut PendingEvents,
    tx: &rusqlite::Transaction<'_>,
    work_item_id: &str,
    pr_url: &str,
    observed_at: &str,
) -> Result<()> {
    if record_in_tx(tx, work_item_id, pr_url, observed_at)? {
        let root = chain_helpers::chain_root(tx, work_item_id)?;
        board_notifications::stage_task_board_change(pending, tx, &root)?;
    }
    Ok(())
}
