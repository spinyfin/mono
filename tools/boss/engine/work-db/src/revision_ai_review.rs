//! Shared presentation of held revisions throughout a started AI review cycle.
//! Pool admission alone is queued; consolidation, verdict apply and reviewer
//! re-admission after a started attempt remain Reviewing until the cycle ends.

use super::*;
use boss_protocol::ReviewStage;

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
    let needs_readmission = super::review_batches::pre_merge_review_needs_readmission_sql("c.ancestor_id");
    let mut stmt = conn.prepare(&format!(
        "WITH RECURSIVE chain(revision_id, ancestor_id, implementation_finished_at) AS (
            SELECT t.id, t.id, (SELECT COALESCE(e.finished_at, e.created_at) FROM work_executions e
                WHERE e.work_item_id = t.id AND e.kind = 'revision_implementation'
                ORDER BY e.created_at DESC, e.id DESC LIMIT 1) FROM tasks t
            WHERE t.kind = 'revision' AND t.status = 'active' AND t.deleted_at IS NULL
              AND COALESCE(t.pr_url, '') != ''
              AND (SELECT e.status FROM work_executions e
                   WHERE e.work_item_id = t.id AND e.kind = 'revision_implementation'
                   ORDER BY e.created_at DESC, e.id DESC LIMIT 1) = 'completed'
            UNION
            SELECT c.revision_id, t.parent_task_id, c.implementation_finished_at FROM chain c
            JOIN tasks t ON t.id = c.ancestor_id WHERE t.parent_task_id IS NOT NULL
        ), stages AS (
            SELECT c.revision_id, CASE b.status WHEN 'applying' THEN 3
                       WHEN 'supervising' THEN 2 ELSE 1 END AS stage
            FROM chain c JOIN pr_review_batches b ON b.cycle_root_id = c.ancestor_id
            WHERE b.phase = 'pre_merge' AND b.created_at >= c.implementation_finished_at
              AND b.status NOT IN ('completed', 'failed')
              AND (b.status IN ('supervising', 'applying') OR EXISTS (
                  SELECT 1 FROM pr_review_batches started
                  JOIN pr_review_batch_members m ON m.batch_id = started.id
                  LEFT JOIN work_executions e ON e.id = m.execution_id
                  WHERE started.cycle_root_id = b.cycle_root_id AND started.phase = 'pre_merge'
                    AND started.created_at >= c.implementation_finished_at
                    AND (e.started_at IS NOT NULL OR e.status = 'running' OR m.status = 'reported')))
            UNION ALL
            SELECT c.revision_id, 1 FROM chain c
            WHERE ({needs_readmission}) AND EXISTS (
                SELECT 1 FROM pr_review_batches started
                JOIN pr_review_batch_members m ON m.batch_id = started.id
                LEFT JOIN work_executions e ON e.id = m.execution_id
                WHERE started.cycle_root_id = c.ancestor_id AND started.phase = 'pre_merge'
                  AND started.created_at >= c.implementation_finished_at
                  AND (e.started_at IS NOT NULL OR e.status = 'running' OR m.status = 'reported'))
            UNION ALL
            SELECT c.revision_id, CASE WHEN EXISTS (
                SELECT 1 FROM pr_review_batch_members m WHERE m.execution_id = e.id AND m.role = 'supervisor'
            ) THEN 2 ELSE 1 END
            FROM chain c JOIN work_executions e ON e.work_item_id = c.ancestor_id
            WHERE e.kind = 'pr_review' AND e.status = 'running'
        ) SELECT revision_id, MAX(stage)
          FROM stages GROUP BY revision_id",
    ))?;
    let rows = stmt.query_map([], |row| {
        let stage = match row.get::<_, i64>(1)? {
            3 => ReviewStage::Applying,
            2 => ReviewStage::Consolidation,
            _ => ReviewStage::Reviewers,
        };
        Ok((row.get(0)?, stage.as_str().to_owned()))
    })?;
    rows.collect()
}
