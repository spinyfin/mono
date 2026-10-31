//! Which revisions are waiting on, and being worked by, the automated review
//! of their PR.
//!
//! A revision's implementation run ends when it pushes its commit. Its card
//! then stays in Doing, held `active` with the PR URL stamped on it, until the
//! consolidated verdict for that push lands and releases it to `in_review`
//! (where it folds into its parent card). While the review is actually under
//! way the revision is being worked on by agents, so the board groups it with
//! the other active Doing work rather than with the cards waiting for a worker
//! slot.

use super::*;

impl WorkDb {
    /// Ids of revisions held in Doing by an automated review that is running
    /// right now.
    ///
    /// A revision qualifies when it is `active`, its latest
    /// `revision_implementation` execution completed (so nothing is
    /// implementing or about to re-implement it), it carries the PR URL the
    /// hold stamps, and its review cycle has live review work: a pre-merge
    /// review batch that has not finished (reviewers, consolidation or
    /// verdict apply) on the chain root, or a non-terminal `pr_review`
    /// execution on the revision or the chain root. A held revision whose
    /// review is not running (the reviewer was reaped and awaits
    /// re-admission) is not listed: nothing is working on it.
    pub fn list_revisions_under_ai_review(&self) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let held = held_revision_ids(&conn)?;
        let mut under_review = Vec::new();
        for id in held {
            let root = chain_root(&conn, &id)?;
            if review_is_running(&conn, &id, &root)? {
                under_review.push(id);
            }
        }
        Ok(under_review)
    }
}

fn held_revision_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT t.id FROM tasks t
         WHERE t.kind = 'revision' AND t.status = 'active' AND t.deleted_at IS NULL
           AND t.pr_url IS NOT NULL AND t.pr_url != ''
           AND (SELECT e.status FROM work_executions e
                WHERE e.work_item_id = t.id AND e.kind = 'revision_implementation'
                ORDER BY e.created_at DESC, e.id DESC LIMIT 1) = 'completed'",
    )?;
    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

fn review_is_running(conn: &Connection, revision_id: &str, root_id: &str) -> Result<bool> {
    let running: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM pr_review_batches
             WHERE cycle_root_id = ?1 AND phase = 'pre_merge'
               AND status NOT IN ('completed', 'failed'))
         OR EXISTS(
             SELECT 1 FROM work_executions
             WHERE kind = 'pr_review' AND work_item_id IN (?1, ?2)
               AND status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned'))",
        params![root_id, revision_id],
        |row| row.get(0),
    )?;
    Ok(running)
}
