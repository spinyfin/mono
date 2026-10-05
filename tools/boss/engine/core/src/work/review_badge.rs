use super::*;

/// Resolve against the observed PR head, across legacy revision-owned and
/// modern cycle-root verdicts. Missing heads and late old-head results never
/// establish a clean review. This is a single batched query for the tree.
pub(super) fn current_head_review_states(
    conn: &Connection,
    ids: &[String],
) -> Result<std::collections::HashMap<String, (&'static str, Option<String>)>> {
    if ids.is_empty() {
        return Ok(Default::default());
    }
    let placeholders = (1..=ids.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
    let outcomes = super::review_verdicts::INFORMATIVE_GATE_OUTCOMES
        .iter()
        .map(|outcome| format!("'{outcome}'"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "WITH RECURSIVE ancestors(card, id, depth) AS (
            SELECT id, id, 0 FROM tasks WHERE id IN ({placeholders})
            UNION ALL
            SELECT a.card, t.parent_task_id, a.depth + 1
            FROM ancestors a JOIN tasks t ON t.id = a.id
            WHERE t.kind = 'revision' AND t.parent_task_id IS NOT NULL AND a.depth < 64
         ), owners AS (
            SELECT a.card, t.id FROM ancestors a JOIN tasks t ON t.id = a.id
            WHERE t.kind != 'revision'
         ), family(root, id, depth) AS (
            SELECT DISTINCT id, id, 0 FROM owners
            UNION ALL
            SELECT f.root, t.id, f.depth + 1 FROM family f
            JOIN tasks t ON t.parent_task_id = f.id
            WHERE t.kind = 'revision' AND f.depth < 64
         )
         SELECT o.card, v.gate_outcome, v.revision_task_id,
                root.ci_required_state, root.pr_mergeable_state,
                root.status = 'active' OR EXISTS(SELECT 1 FROM family f JOIN tasks t ON t.id = f.id
                       WHERE f.root = root.id AND t.kind = 'revision'
                         AND t.deleted_at IS NULL AND t.status IN ('todo', 'active', 'blocked'))
         FROM owners o JOIN tasks root ON root.id = o.id
         LEFT JOIN pr_review_verdicts v ON v.id = (
             SELECT rv.id FROM pr_review_verdicts rv
             JOIN family f ON f.id = rv.work_item_id AND f.root = root.id
             WHERE rv.head_sha = NULLIF(root.pr_head_sha, '')
               AND rv.gate_outcome IN ({outcomes})
             ORDER BY rv.created_at DESC, rv.id DESC LIMIT 1
         )
         WHERE NULLIF(root.pr_url, '') IS NOT NULL"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, bool>(5)?,
        ))
    })?;
    let mut result = std::collections::HashMap::new();
    for row in rows {
        let (id, outcome, revision, ci, mergeable, pending_work) = row?;
        let (state, revision) = match outcome.as_deref() {
            Some(REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS | REVIEW_GATE_OUTCOME_REVISION_CREATION_FAILED) => {
                (AI_REVIEW_STATE_REVIEWED_WITH_FINDINGS, revision)
            }
            Some(REVIEW_GATE_OUTCOME_COMPLETED_CLEAN) => {
                let state =
                    if pending_work || ci.as_deref() != Some("success") || mergeable.as_deref() != Some("mergeable") {
                        AI_REVIEW_STATE_REVIEWED_CLEAN_PENDING
                    } else {
                        AI_REVIEW_STATE_REVIEWED_ALL_CLEAR
                    };
                (state, None)
            }
            _ => (AI_REVIEW_STATE_NOT_REVIEWED, None),
        };
        result.insert(id, (state, revision));
    }
    Ok(result)
}
