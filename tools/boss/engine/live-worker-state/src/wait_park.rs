//! Park/resume bookkeeping for a declared wait, keyed by declaration
//! identity so a later wait cannot inherit an earlier park's resume flag.

use boss_protocol::{LiveWorkerState, WorkerActivity};

use super::SlotMeta;
use super::declared_wait::{DeclaredWait, DeclaredWaitIdentity, SharedWaitSource, effective_wait, settle_parked};

#[derive(Clone, Copy)]
pub(super) struct WaitSnapshot {
    pub wait: DeclaredWait,
    pub identity: Option<DeclaredWaitIdentity>,
}

pub(super) fn wait_snapshot(source: &Option<SharedWaitSource>, run_id: &str, now_epoch_secs: i64) -> WaitSnapshot {
    match source {
        Some(source) => WaitSnapshot {
            wait: source.declared_wait(run_id, now_epoch_secs),
            identity: source.wait_identity(run_id),
        },
        None => WaitSnapshot {
            wait: DeclaredWait::None,
            identity: None,
        },
    }
}

/// Fold a declared wait into a parked slot, scoping a lapsed wait to the
/// park it applied to so a later resume cannot stay overdue forever.
pub(super) fn settle_slot(
    state: &mut LiveWorkerState,
    meta: &mut SlotMeta,
    raw: WorkerActivity,
    snapshot: WaitSnapshot,
) {
    observe_wait_declaration(meta, snapshot);
    let wait = effective_wait(snapshot.wait, lapsed_wait_consumed(meta, snapshot.identity.as_ref()));
    state.activity = settle_parked(raw, meta.blocking_prompt_pending, wait);
    note_wait_park(meta, state.activity, snapshot.identity);
}

pub(super) fn observe_wait_declaration(meta: &mut SlotMeta, snapshot: WaitSnapshot) {
    if snapshot.wait == DeclaredWait::None {
        return;
    }
    let Some(identity) = snapshot.identity else {
        return;
    };
    if meta
        .parked_wait
        .is_some_and(|parked| parked.generation != identity.generation)
    {
        meta.parked_on_declared_wait = false;
        meta.resumed_after_wait_park = false;
        meta.parked_wait = None;
    }
}

pub(super) fn lapsed_wait_consumed(meta: &SlotMeta, identity: Option<&DeclaredWaitIdentity>) -> bool {
    let resumed_after_this_wait = meta.resumed_after_wait_park
        && match (meta.parked_wait, identity) {
            (_, None) => true,
            (None, Some(_)) => true,
            (Some(parked), Some(current)) => parked.generation == current.generation,
        };
    let worked_past_declaration = match (
        meta.last_main_activity_seq,
        identity.and_then(|id| id.declared_after_seq),
    ) {
        (Some(main), Some(declared)) => main > declared,
        _ => false,
    };
    resumed_after_this_wait || worked_past_declaration
}

pub(super) fn note_wait_park(meta: &mut SlotMeta, activity: WorkerActivity, identity: Option<DeclaredWaitIdentity>) {
    if matches!(activity, WorkerActivity::WaitingOnWork | WorkerActivity::WaitOverdue) {
        meta.parked_on_declared_wait = true;
        meta.resumed_after_wait_park = false;
        meta.parked_wait = identity;
    }
}

pub(super) fn note_wait_resume(meta: &mut SlotMeta, seq: Option<u64>, identity: Option<&DeclaredWaitIdentity>) {
    if !meta.parked_on_declared_wait {
        return;
    }
    if let (Some(seq), Some(id)) = (seq, identity) {
        if meta
            .parked_wait
            .is_some_and(|parked| parked.generation != id.generation)
        {
            return;
        }
        if id.declared_after_seq.is_some_and(|declared| seq <= declared) {
            return;
        }
    }
    meta.resumed_after_wait_park = true;
}

/// A late `Stop` overtaken by already-applied main-session activity still
/// opened a wait park in emission order. Record that park as already resumed.
pub(super) fn note_overtaken_wait_park(meta: &mut SlotMeta, stop_seq: u64, snapshot: WaitSnapshot) {
    if snapshot.wait == DeclaredWait::None {
        return;
    }
    if snapshot
        .identity
        .and_then(|id| id.declared_after_seq)
        .is_some_and(|declared| declared >= stop_seq)
    {
        return;
    }
    meta.parked_on_declared_wait = true;
    meta.parked_wait = snapshot.identity;
    meta.resumed_after_wait_park = true;
}
