//! Releasing the revisions a review decision settles.
//!
//! A revision never opens its own PR. When its run ends, the engine holds the
//! task `active` until a review decision on the push it contributed releases
//! it. That decision is either a verdict applied for a batch, or a batch that
//! failed terminally. Both find the revision the same way, through the head
//! the revision's latest completed run is recorded as having contributed
//! (`work_executions.revision_stop_contributed_head`), so the lookup lives
//! here once rather than being copied into each decision path.
//!
//! A revision whose run recorded no head at all can never be found by that
//! lookup. That is a defect somewhere upstream (a path that created a review
//! for the revision without recording the head), and left alone it strands the
//! revision `active` with nothing in the logs or on the board. After every
//! decision, [`report_unplaceable_held_revisions_in_tx`] therefore names each
//! such revision in the log and raises an attention item on it.

use anyhow::Result;
use rusqlite::Transaction;

use boss_engine_storage_values::attention_kinds::PR_REVIEW_HELD_REVISION_UNPLACEABLE_ATTENTION_KIND;

/// Revision tasks of `cycle_root_id`'s chain, plus the root itself (which the
/// `kind = 'revision'` filter then excludes), as a bound-parameter list.
fn chain_task_ids(tx: &Transaction<'_>, cycle_root_id: &str) -> Result<Vec<String>> {
    // Candidate revision rows are sourced from the full review-cycle chain
    // under `cycle_root_id`, not just its direct children: residual
    // pre-flatten-migration rows can still nest a revision under another
    // revision, and a direct `parent_task_id = cycle_root_id` predicate would
    // exclude such a row before its contributed head could even be checked.
    // The helper intentionally traverses tombstoned intermediate revisions so
    // a live descendant is still found.
    let mut ids = super::chain_helpers::collect_chain_revision_ids_including_deleted(tx, cycle_root_id)?;
    ids.push(cycle_root_id.to_owned());
    Ok(ids)
}

fn in_list_placeholders(count: usize) -> String {
    (1..=count)
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn query_ids(tx: &Transaction<'_>, sql: &str, values: &[&dyn rusqlite::ToSql]) -> Result<Vec<String>> {
    let mut statement = tx.prepare(sql)?;
    Ok(statement
        .query_map(values, |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Every `active` revision of the chain held for exactly the push at
/// `target_sha`: its latest completed `revision_implementation` run recorded
/// `target_sha` as the head it contributed.
///
/// A revision already superseded by a later push has a different recorded head
/// and is left alone; a verdict for its own, later push releases it instead.
/// Admission can also record a later head on a superseded revision (see
/// `review_batches::record_awaited_review_head_in_tx`), and several revisions
/// can be held for the same head, so more than one id can come back.
pub(crate) fn held_revision_ids_for_head_in_tx(
    tx: &Transaction<'_>,
    cycle_root_id: &str,
    target_sha: &str,
) -> Result<Vec<String>> {
    let ids = chain_task_ids(tx, cycle_root_id)?;
    let sql = format!(
        "SELECT t.id FROM tasks t
         JOIN work_executions we ON we.id = (
             SELECT latest.id FROM work_executions latest
             WHERE latest.work_item_id = t.id
               AND latest.kind = 'revision_implementation'
               AND latest.status = 'completed'
             ORDER BY latest.created_at DESC, latest.id DESC LIMIT 1
         )
         WHERE t.id IN ({placeholders}) AND t.kind = 'revision'
           AND t.status = 'active' AND t.deleted_at IS NULL
           AND we.revision_stop_contributed_head = ?{head}
         ORDER BY t.id",
        placeholders = in_list_placeholders(ids.len()),
        head = ids.len() + 1,
    );
    let mut values: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    values.push(&target_sha);
    query_ids(tx, &sql, &values)
}

/// Every `active` revision of the chain that finished its run, carries the
/// PR, and has no recorded contributed head, so no review decision can ever
/// find it. A revision with an unfinished run is not held: that run stamps its
/// own head at its Stop boundary.
fn unplaceable_held_revision_ids_in_tx(tx: &Transaction<'_>, cycle_root_id: &str) -> Result<Vec<String>> {
    let ids = chain_task_ids(tx, cycle_root_id)?;
    let sql = format!(
        "SELECT t.id FROM tasks t
         JOIN work_executions we ON we.id = (
             SELECT latest.id FROM work_executions latest
             WHERE latest.work_item_id = t.id
               AND latest.kind = 'revision_implementation'
               AND latest.status = 'completed'
             ORDER BY latest.created_at DESC, latest.id DESC LIMIT 1
         )
         WHERE t.id IN ({placeholders}) AND t.kind = 'revision'
           AND t.status = 'active' AND t.deleted_at IS NULL
           AND t.pr_url IS NOT NULL AND t.pr_url != ''
           AND (we.revision_stop_contributed_head IS NULL OR we.revision_stop_contributed_head = '')
           AND NOT EXISTS (
                 SELECT 1 FROM work_executions unfinished
                 WHERE unfinished.work_item_id = t.id
                   AND unfinished.kind = 'revision_implementation'
                   AND unfinished.status NOT IN ('completed', 'abandoned', 'failed', 'cancelled', 'orphaned')
               )
         ORDER BY t.id",
        placeholders = in_list_placeholders(ids.len()),
    );
    let values: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    query_ids(tx, &sql, &values)
}

/// Name, in the log and as an attention item on the revision, every held
/// revision of the chain that the review decision at `target_sha` could not
/// have released because it has no recorded contributed head.
///
/// `decision` says which path was deciding (a verdict, or a failed batch), for
/// the log and the item body. The item is resolved by the release that finally
/// moves the revision (see [`resolve_unplaceable_attention_in_tx`]).
pub(crate) fn report_unplaceable_held_revisions_in_tx(
    tx: &Transaction<'_>,
    cycle_root_id: &str,
    target_sha: &str,
    decision: &str,
) -> Result<()> {
    for revision_id in unplaceable_held_revision_ids_in_tx(tx, cycle_root_id)? {
        tracing::warn!(
            revision_id,
            cycle_root_id,
            target_sha,
            decision,
            "review decision could not release a held revision: its latest completed run recorded no \
             contributed head, so no review can match it; raising an attention item",
        );
        super::workitems::upsert_refreshing_work_item_attention_in_tx(
            tx,
            &revision_id,
            PR_REVIEW_HELD_REVISION_UNPLACEABLE_ATTENTION_KIND,
            "Revision is still active after its review settled",
            &format!(
                "A review of this PR settled at `{target_sha}` ({decision}), but this revision is still held \
                 `active` waiting for a review decision. Its latest completed run never recorded the PR head it \
                 pushed, so a review decision cannot tell that this revision is the push it covers and cannot \
                 release it. Inspect the revision's last run and the PR's review history, then move the \
                 revision on once its push is known to be reviewed."
            ),
        )?;
    }
    Ok(())
}

/// Resolve the unplaceable-revision item on `revision_id`, because the
/// revision has been released or can now be found by the review it awaits.
pub(crate) fn resolve_unplaceable_attention_in_tx(tx: &Transaction<'_>, revision_id: &str) -> Result<()> {
    super::resolve_attention_kind_in_tx(tx, revision_id, PR_REVIEW_HELD_REVISION_UNPLACEABLE_ATTENTION_KIND)?;
    Ok(())
}
