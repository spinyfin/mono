//! Shared presentation of held revisions throughout a started AI review cycle.
//! Pool admission alone is queued; consolidation, verdict apply and reviewer
//! re-admission after a started attempt remain Reviewing until the cycle ends.

use super::*;

impl WorkDb {
    /// Held revisions whose review cycle has started and has not finished.
    pub fn list_revisions_under_ai_review(&self) -> Result<Vec<String>> {
        let conn = self.connect()?;
        Ok(held_review_stages(&conn)?.into_keys().collect())
    }
}

/// One query for all held revisions, including arbitrarily deep revision chains.
/// Both Doing membership and task captions consume this projection.
pub(super) fn held_review_stages(conn: &Connection) -> rusqlite::Result<std::collections::HashMap<String, String>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE chain(revision_id, ancestor_id) AS (
            SELECT t.id, t.id FROM tasks t
            WHERE t.kind = 'revision' AND t.status = 'active' AND t.deleted_at IS NULL
              AND COALESCE(t.pr_url, '') != ''
              AND (SELECT e.status FROM work_executions e
                   WHERE e.work_item_id = t.id AND e.kind = 'revision_implementation'
                   ORDER BY e.created_at DESC, e.id DESC LIMIT 1) = 'completed'
            UNION
            SELECT c.revision_id, t.parent_task_id FROM chain c
            JOIN tasks t ON t.id = c.ancestor_id WHERE t.parent_task_id IS NOT NULL
        ), stages AS (
            SELECT c.revision_id, CASE b.status WHEN 'applying' THEN 3
                       WHEN 'supervising' THEN 2 ELSE 1 END AS stage
            FROM chain c JOIN pr_review_batches b ON b.cycle_root_id = c.ancestor_id
            WHERE b.phase = 'pre_merge' AND b.status NOT IN ('completed', 'failed')
              AND (b.status IN ('supervising', 'applying') OR EXISTS (
                  SELECT 1 FROM pr_review_batch_members m
                  LEFT JOIN work_executions e ON e.id = m.execution_id
                  WHERE m.batch_id = b.id
                    AND (e.started_at IS NOT NULL OR e.status = 'running' OR m.status = 'reported')))
            UNION ALL
            SELECT c.revision_id, CASE WHEN EXISTS (
                SELECT 1 FROM pr_review_batch_members m WHERE m.execution_id = e.id AND m.role = 'supervisor'
            ) THEN 2 ELSE 1 END
            FROM chain c JOIN work_executions e ON e.work_item_id = c.ancestor_id
            WHERE e.kind = 'pr_review' AND e.status = 'running'
        ) SELECT revision_id, CASE MAX(stage) WHEN 3 THEN 'applying'
                   WHEN 2 THEN 'consolidation' ELSE 'reviewers' END
          FROM stages GROUP BY revision_id",
    )?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}
