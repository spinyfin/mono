//! Which candidate a pass attends to next, remembered across passes.
//!
//! Every candidate list is rebuilt from the database each pass, oldest
//! `updated_at` first. Walked in that order, a pass that runs out of time
//! always drops the same tail — and the tail is the most recently updated
//! rows, the ones most likely to have just merged. The next pass re-lists
//! in the same order and drops them again, so they are never observed for
//! as long as passes stay slow.
//!
//! A [`Rotation`] remembers when each candidate was last attended to and
//! orders a list least-recently-attended first, so whatever one pass did
//! not reach is what the next pass starts with.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::pass_trace::PassTrace;

/// Least-recently-attended ordering over one family of candidates, keyed by
/// a stable id (a work item id, an execution id).
///
/// A candidate is stamped when the pass *starts* on it, not when it
/// finishes. A pass cut off inside one candidate therefore leaves that
/// candidate most-recently-attended, at the back of the next pass's order:
/// a candidate whose handling is itself what exhausts the budget cannot
/// hold the front of the order and starve the rest.
#[derive(Debug, Default)]
pub(crate) struct Rotation {
    clock: u64,
    attended_at: HashMap<String, u64>,
}

impl Rotation {
    /// Record that the pass is starting on `key`.
    pub(crate) fn stamp(&mut self, key: &str) {
        self.clock += 1;
        self.attended_at.insert(key.to_owned(), self.clock);
    }

    /// `items` reordered least-recently-attended first. A candidate never
    /// attended to sorts ahead of every one that has been; ties keep their
    /// incoming (list) order.
    pub(crate) fn order<T>(&self, mut items: Vec<T>, key: impl Fn(&T) -> &str) -> Vec<T> {
        items.sort_by_key(|item| self.attended_at.get(key(item)).copied().unwrap_or(0));
        items
    }

    /// Forget every candidate not in `live`, so the memory stays the size
    /// of the current candidate set. One that later returns is new again.
    pub(crate) fn retain(&mut self, live: &HashSet<&str>) {
        self.attended_at.retain(|key, _| live.contains(key.as_str()));
    }

    #[cfg(test)]
    pub(crate) fn tracked(&self) -> usize {
        self.attended_at.len()
    }
}

/// Rotations for the upkeep subphases that walk a list.
#[derive(Debug, Default)]
pub(crate) struct UpkeepRotations {
    /// Stranded ci-remediation rescue, keyed by work item id.
    pub(crate) stranded_ci: Rotation,
    /// Stranded-blocked reconcile, keyed by work item id.
    pub(crate) stranded_blocked: Rotation,
    /// Reviewer-fallback for stalled reviews, keyed by task id.
    pub(crate) stalled_reviewer: Rotation,
}

/// The rotations one poller loop keeps, one per candidate family.
#[derive(Debug, Default)]
pub(crate) struct PassRotations {
    /// The PR candidate walk, keyed by work item id.
    pub(crate) walk: Rotation,
    /// The local upkeep subphases' item lists.
    pub(crate) upkeep: UpkeepRotations,
    /// The live-worker PR-detection recheck, keyed by execution id.
    pub(crate) recheck: Rotation,
    /// Background-nudge rechecks, keyed by execution id.
    pub(crate) background_nudge: Rotation,
    /// The late-PR recheck of terminal executions, keyed by execution id.
    pub(crate) late_pr: Rotation,
}

/// What a poller loop carries from one full detection pass to the next:
/// the phase timing of the pass in flight and the rotation memory. Held
/// outside the pass future, so both survive a pass that is dropped at its
/// budget.
#[derive(Debug, Default)]
pub struct PassState {
    pub(crate) trace: Arc<PassTrace>,
    pub(crate) probe_order: Arc<Mutex<crate::probe_budget::ProbeOrder>>,
    rotations: Mutex<PassRotations>,
    turns: PhaseTurns,
    /// What the probe tiers remember between passes: when each tracked PR was
    /// last observed, and which products are active. See [`crate::probe_tier`].
    tiering: Mutex<crate::probe_tier::TierState>,
}

#[derive(Debug, Default)]
struct PhaseTurns {
    /// Starting offset among the post-terminal groups (open walk, local
    /// upkeep, per-execution families). Advanced at the start of that work,
    /// before any of it runs, so a pass dropped in the open walk still
    /// rotates.
    phase_turn: AtomicU64,
    /// Starting offset among the three per-execution families. Advanced
    /// only when that group actually has time, so a family is not charged
    /// for a pass that yielded before it started.
    late_family_turn: AtomicU64,
    /// Starting offset among the six local upkeep subphases. Advanced when
    /// the upkeep group starts, before any of its awaited work.
    upkeep_turn: AtomicU64,
}

impl PassState {
    /// Phase timing of the pass in flight, or of the pass that just ended.
    pub fn trace(&self) -> &PassTrace {
        &self.trace
    }

    /// Name the products whose `in_review` PRs the next passes probe in full
    /// every pass; the PRs of every other product are probed in full only when
    /// something else calls for it (see [`crate::probe_tier`]) and are
    /// otherwise watched for merge or close on a slower cadence. `None`, or an
    /// empty set, singles out no product: every product is active.
    pub fn set_active_products(&self, products: Option<HashSet<String>>) {
        self.tiering().active_products = products.filter(|products| !products.is_empty());
    }

    /// The probe-tier memory. Never held across an await.
    pub(crate) fn tiering(&self) -> MutexGuard<'_, crate::probe_tier::TierState> {
        self.tiering.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The rotation memory. Never held across an await.
    pub(crate) fn rotations(&self) -> MutexGuard<'_, PassRotations> {
        // Plain bookkeeping with no invariant a panic could break.
        self.rotations.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Take the next starting offset among post-terminal groups. Call once
    /// per pass, after the terminal walk and before any rotated group runs.
    pub(crate) fn next_phase_turn(&self) -> u64 {
        self.turns.phase_turn.fetch_add(1, Ordering::Relaxed)
    }

    /// The current starting offset among the per-execution families,
    /// without advancing it.
    pub(crate) fn late_family_turn(&self) -> u64 {
        self.turns.late_family_turn.load(Ordering::Relaxed)
    }

    /// Take the next starting offset among the upkeep subphases. Call when
    /// the upkeep group has time to run, before its first awaited work.
    pub(crate) fn next_upkeep_turn(&self) -> u64 {
        self.turns.upkeep_turn.fetch_add(1, Ordering::Relaxed)
    }

    /// Take the next starting offset among PR recheck, background nudges
    /// and late PR. Call when the per-execution group has time to run.
    pub(crate) fn next_late_family_turn(&self) -> u64 {
        self.turns.late_family_turn.fetch_add(1, Ordering::Relaxed)
    }
}
