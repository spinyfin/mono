//! Shared rank-and-rewrite skeleton for merge-queue `section_order`
//! renumbering passes.
//!
//! Both the GitHub-native path (`merge_poller::renumber_merge_queue`) and
//! the Trunk path (`trunk_queue_poller::renumber_trunk_merge_queue`) load
//! every queued member of a product, sort it by a mechanism-specific key,
//! and rewrite + re-broadcast only the rows whose rank actually changed.
//! This module owns that skeleton once; each mechanism supplies its own
//! parse/sort/unchanged-check/rebuild so the two `merge_queue_detail` JSON
//! shapes (GitHub's `{position, state, enqueued_at, section_order}` vs.
//! Trunk's `{source, state, position, enqueued_at, queue_state,
//! section_order}`) stay distinct without duplicating the load-sort-diff-
//! write loop around them.

use crate::coordinator::ExecutionPublisher;
use crate::work::{QueuedMergeQueueMember, WorkDb};

/// The invariant, non-closure part of a [`renumber_section_order`] call —
/// split out purely to keep the function's argument count under the
/// repo's clippy threshold; every field is a straight passthrough.
pub(crate) struct RenumberContext<'a> {
    pub work_db: &'a WorkDb,
    pub publisher: &'a dyn ExecutionPublisher,
    pub product_id: &'a str,
    /// Prefixes this pass's own tracing lines (`"merge poller"` /
    /// `"trunk queue poller"`) so a shared helper's logs still read as
    /// belonging to the calling mechanism.
    pub log_context: &'a str,
    /// `FrontendEvent` reason string passed to `publish_work_item_changed`.
    pub event: &'a str,
}

/// Recompute a canonical `1..N` rank for every parsed member of
/// `ctx.product_id`'s queue, and rewrite + re-broadcast the rows whose
/// rank changed.
///
/// - `parse` decodes a raw DB row into `(task_id, T)`; returning `None`
///   drops the row from the ranked set entirely (e.g. a row belonging to
///   the other mechanism).
/// - `sort_key` orders the parsed members into their canonical rank.
/// - `unchanged` reports whether `T`'s already-stored rank-bearing field
///   matches the freshly computed rank, so a member already at its
///   canonical position is left untouched (the "only touch what changed"
///   invariant both callers rely on).
/// - `rebuild` re-serializes `T` with the freshly computed rank baked in.
///
/// Returns the number of rows actually rewritten, so a caller that tracks
/// its own pass-level write counter (e.g. `TrunkSweepOutcome::state_writes`)
/// can fold it in without threading `&mut` state through these closures.
pub(crate) async fn renumber_section_order<T, K: Ord>(
    ctx: &RenumberContext<'_>,
    parse: impl Fn(QueuedMergeQueueMember) -> Option<(String, T)>,
    sort_key: impl Fn(&T, &str) -> K,
    unchanged: impl Fn(&T, i64) -> bool,
    rebuild: impl Fn(&T, i64) -> Option<String>,
) -> usize {
    let members = match ctx.work_db.list_queued_merge_queue_members(ctx.product_id) {
        Ok(members) => members,
        Err(err) => {
            tracing::warn!(
                product_id = ctx.product_id,
                ?err,
                log_context = ctx.log_context,
                "merge queue renumber: failed to list queued members",
            );
            return 0;
        }
    };

    let mut parsed: Vec<(String, T)> = members.into_iter().filter_map(parse).collect();
    if parsed.is_empty() {
        return 0;
    }
    parsed.sort_by_key(|(task_id, detail)| sort_key(detail, task_id));

    let mut writes = 0usize;
    for (rank, (task_id, detail)) in parsed.iter().enumerate() {
        let position = (rank + 1) as i64;
        if unchanged(detail, position) {
            continue;
        }
        let Some(json) = rebuild(detail, position) else {
            continue;
        };
        match ctx.work_db.update_task_merge_queue_detail(task_id, &json) {
            Ok(true) => {
                writes += 1;
                ctx.publisher
                    .publish_work_item_changed(ctx.product_id, task_id, ctx.event)
                    .await;
                tracing::debug!(
                    work_item_id = %task_id,
                    product_id = ctx.product_id,
                    position,
                    log_context = ctx.log_context,
                    "merge queue renumber: renumbered position",
                );
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(
                work_item_id = %task_id,
                product_id = ctx.product_id,
                ?err,
                log_context = ctx.log_context,
                "merge queue renumber: failed to persist renumbered position",
            ),
        }
    }
    writes
}
