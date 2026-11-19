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
use std::sync::{Mutex, MutexGuard};

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

/// The rotations one poller loop keeps, one per candidate family.
#[derive(Debug, Default)]
pub(crate) struct PassRotations {
    /// The PR candidate walk, keyed by work item id.
    pub(crate) walk: Rotation,
    /// The live-worker PR-detection recheck, keyed by execution id.
    pub(crate) recheck: Rotation,
    /// The late-PR recheck of terminal executions, keyed by execution id.
    pub(crate) late_pr: Rotation,
}

/// What a poller loop carries from one full detection pass to the next:
/// the phase timing of the pass in flight and the rotation memory. Held
/// outside the pass future, so both survive a pass that is dropped at its
/// budget.
#[derive(Debug, Default)]
pub struct PassState {
    pub(crate) trace: PassTrace,
    rotations: Mutex<PassRotations>,
}

impl PassState {
    /// Phase timing of the pass in flight, or of the pass that just ended.
    pub fn trace(&self) -> &PassTrace {
        &self.trace
    }

    /// The rotation memory. Never held across an await.
    pub(crate) fn rotations(&self) -> MutexGuard<'_, PassRotations> {
        // Plain bookkeeping with no invariant a panic could break.
        self.rotations.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
