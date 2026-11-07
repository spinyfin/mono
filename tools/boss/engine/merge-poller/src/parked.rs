//! Parking at the merge poller: which reactions a parked row keeps.
//!
//! A parked row (see `boss_work_db::parking`) atrophies: no upkeep runs for it.
//! The poller's candidate lists are shared between two different duties, which
//! is why the exclusion cannot live in their SQL the way it does for the
//! engine's other producers:
//!
//! - **Terminal facts.** The PR merged, or it was closed without merging. These
//!   are facts about the world, not upkeep, so a parked `in_review` (or
//!   `blocked`) row is still listed, still probed, and still moves to `done` /
//!   takes the closed-unmerged path.
//! - **Upkeep.** Everything the poller does about an *open* PR — conflict and
//!   CI remediation, moot-revision retirement, review-guide refresh, poll-state
//!   and merge-queue upkeep, Trunk adoption and eviction handling, the
//!   post-merge deep review. None of it runs for a parked row.
//!
//! [`candidate_is_parked`] is the single question both the sweep and the
//! per-PR watchers ask to tell the two apart.

use crate::work::{PendingMergeCheck, WorkDb};

/// `true` when `candidate`'s work item is parked, so only terminal facts may be
/// recorded for it. A failed read is treated as "not parked" and logged: the
/// same fail-open stance the watchers' opt-out gate takes, so a transient
/// lookup error does not silently drop a real signal for an un-parked row.
pub fn candidate_is_parked(work_db: &WorkDb, candidate: &PendingMergeCheck) -> bool {
    match work_db.is_parked(&candidate.work_item_id) {
        Ok(parked) => parked,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "merge poller: failed to read the parked state; treating the row as not parked",
            );
            false
        }
    }
}
