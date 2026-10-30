//! Dispatch order for queued Doing cards, derived from the scheduler's own
//! ready queue so the board and the dispatcher cannot disagree about who is
//! next. Positions travel with the live-worker board snapshot.

use std::collections::BTreeMap;

use boss_protocol::{DispatchPool, QueuedDispatch, WorkExecution};

use crate::coordinator::DispatchPause;
use crate::work::WorkDb;

/// What the scheduler consults before it walks the ready queue: the pause
/// state, and the per-row chain-serialization hold.
pub(super) struct DispatchGate<'a> {
    pub pause: Option<&'a DispatchPause>,
    pub automation_paused: bool,
    pub pool_of: &'a dyn Fn(&WorkExecution) -> DispatchPool,
    /// `true` when the scheduler would defer the row behind a live writer on
    /// its PR chain (the row stays `ready` with a wait reason).
    pub chain_blocked: &'a dyn Fn(&WorkExecution) -> bool,
}

/// One entry per work item with a pending execution. Items with a `ready`
/// execution the dispatcher would take are ranked in the order
/// `WorkDb::list_ready_executions` returns them (the order `drain_ready_queue`
/// walks). Items whose ready rows are all held (a pause, or a live writer on
/// the PR chain) and items that are not on the ready walk at all (waiting on
/// a dependency, or inside a `dispatch_not_before` backoff) still carry their
/// pool but have no position, and sort below every ranked card.
pub(super) fn classify_queued_dispatch(db: &WorkDb, gate: &DispatchGate<'_>) -> BTreeMap<String, QueuedDispatch> {
    let ready = match db.list_ready_executions() {
        Ok(ready) => ready,
        Err(err) => {
            tracing::warn!(
                ?err,
                "queued dispatch order unavailable: failed to list ready executions"
            );
            return BTreeMap::new();
        }
    };
    // Same exemption set the drain reads: critical main-pool rows dispatch
    // through an operator pause.
    let critical = db.list_critical_ready_execution_ids().unwrap_or_default();
    let mut entries: BTreeMap<String, QueuedDispatch> = BTreeMap::new();
    let mut next_position = 0u32;
    for execution in &ready {
        let pool = (gate.pool_of)(execution);
        let held = is_held(gate, pool, critical.contains(&execution.id)) || (gate.chain_blocked)(execution);
        let position = (!held).then(|| {
            let position = next_position;
            next_position += 1;
            position
        });
        let owner = db.owning_work_item_id(&execution.work_item_id);
        // The first ranked row for an item wins; a held row only stands in
        // until a ranked one shows up.
        let keep_existing = entries.get(&owner).is_some_and(|existing| existing.position.is_some());
        if !keep_existing {
            entries.insert(
                owner,
                QueuedDispatch {
                    pool,
                    position,
                    next_up: false,
                },
            );
        }
    }
    match db.list_unranked_pending_executions() {
        Ok(pending) => {
            for execution in &pending {
                entries
                    .entry(db.owning_work_item_id(&execution.work_item_id))
                    .or_insert_with(|| QueuedDispatch {
                        pool: (gate.pool_of)(execution),
                        position: None,
                        next_up: false,
                    });
            }
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                "queued dispatch pools unavailable for unranked pending executions"
            );
        }
    }
    let next_up = entries
        .iter()
        .filter(|(_, entry)| entry.pool == DispatchPool::Main)
        .filter_map(|(id, entry)| entry.position.map(|position| (position, id.clone())))
        .min()
        .map(|(_, id)| id);
    if let Some(entry) = next_up.and_then(|id| entries.get_mut(&id)) {
        entry.next_up = true;
    }
    entries
}

/// Mirrors the pause gates at the top of `drain_ready_queue`'s per-row loop.
/// The interactive concurrency cap and full pools are deliberately absent:
/// they delay a row the dispatcher would otherwise take, they do not reorder it.
fn is_held(gate: &DispatchGate<'_>, pool: DispatchPool, critical: bool) -> bool {
    if gate.automation_paused && pool == DispatchPool::Automation {
        return true;
    }
    let Some(pause) = gate.pause else {
        return false;
    };
    if pause.reviews_held() {
        return true;
    }
    pool != DispatchPool::Review && !(pool == DispatchPool::Main && critical)
}

#[cfg(test)]
#[path = "queued_dispatch_tests.rs"]
mod tests;
