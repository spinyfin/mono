//! Queued probe delivery through the worker's recorded host transport.
use std::sync::Arc;

use boss_execution_contracts::{ControlOutcome, WorkerInputKind};
use boss_protocol::ProbeDeliveryState;

use super::{ServerState, probes::ProbeDispatchOutcome};

pub(super) async fn dispatch(state: &Arc<ServerState>, run: &str) -> Option<ProbeDispatchOutcome> {
    let adapter = match state.spoke_worker_adapter(run).await {
        Ok(None) => return None,
        Ok(Some(adapter)) => adapter,
        Err(error) => {
            tracing::warn!(run, error = %error, "probe host unavailable; leaving probe queued");
            return Some(ProbeDispatchOutcome::RequeuedAfterFailure);
        }
    };
    if state.has_in_flight_probe(run) {
        return Some(ProbeDispatchOutcome::AlreadyInFlight);
    }
    if !state.has_pending_probe(run) {
        return Some(ProbeDispatchOutcome::NothingQueued);
    }
    let (path, offset) = super::worker_events::transcript_offset_for_run(state, run).await;
    let Some(probe) = state.try_reserve_probe_for_delivery(run, path, offset) else {
        return Some(ProbeDispatchOutcome::RacedToEmpty);
    };
    state.set_probe_lifecycle(&probe.probe_id, ProbeDeliveryState::Injected);
    let outcome = adapter
        .deliver_worker_input(run, WorkerInputKind::Probe, &probe.text)
        .await;
    match outcome {
        Ok(ControlOutcome::NotSubmitted(reason) | ControlOutcome::Rejected { reason, .. }) => {
            state.set_probe_lifecycle_detail(&probe.probe_id, ProbeDeliveryState::Queued, Some(reason));
            Some(if state.release_probe_reservation(run, probe) {
                ProbeDispatchOutcome::RequeuedAfterFailure
            } else {
                ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Abandoned)
            })
        }
        // A successful host write does not prove consumption. Keep the
        // reservation for the reply boundary, including transport errors
        // whose side effects cannot be established by this caller.
        Ok(ControlOutcome::Delivered(_)) => {
            state.set_probe_lifecycle_detail(&probe.probe_id, ProbeDeliveryState::Buffered, None);
            Some(ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Buffered))
        }
        Ok(ControlOutcome::Uncertain(reason)) => {
            state.set_probe_lifecycle_detail(&probe.probe_id, ProbeDeliveryState::Unconfirmed, Some(reason));
            Some(ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Unconfirmed))
        }
        Err(error) => {
            state.set_probe_lifecycle_detail(
                &probe.probe_id,
                ProbeDeliveryState::Unconfirmed,
                Some(format!("{error:#}")),
            );
            Some(ProbeDispatchOutcome::Dispatched(ProbeDeliveryState::Unconfirmed))
        }
    }
}

/// Where a spoke-supervised worker is in its turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpokeTurn {
    /// A turn event arrived after the last `Stop`.
    Working,
    /// The last turn event was a `Stop`: the composer is idle.
    Parked,
}

/// The latest turn boundary seen for each spoke-supervised run.
///
/// Turn evidence reaches this map only through `record_spoke_turn_event`, whose
/// sole production caller handles hook events arriving on the engine's local
/// events socket. A spoke host journals its worker's hook events and sends them
/// to the hub as stream records, but the hub does not yet project those records
/// into worker-event dispatch, so no production path records a turn boundary for
/// a spoke run today. Until that projection lands the map stays empty and
/// `spoke_parked_for_delivery` stays closed: mid-turn delivery to a spoke worker
/// is refused rather than guessed at.
#[derive(Default)]
pub(super) struct SpokeTurns(std::sync::Mutex<std::collections::HashMap<String, SpokeTurn>>);

impl SpokeTurns {
    fn record(&self, run: &str, turn: SpokeTurn) {
        if let Ok(mut turns) = self.0.lock() {
            turns.insert(run.to_owned(), turn);
        }
    }

    fn get(&self, run: &str) -> Option<SpokeTurn> {
        self.0.lock().ok().and_then(|turns| turns.get(run).copied())
    }

    pub(super) fn forget(&self, run: &str) {
        if let Ok(mut turns) = self.0.lock() {
            turns.remove(run);
        }
    }
}

/// Mid-turn delivery to a spoke worker is allowed only once a turn's `Stop` has
/// been recorded and nothing since. A worker with no recorded turn boundary is
/// not known to be idle, so it is refused; as the type's docs explain, that is
/// every spoke worker until stream-record projection exists.
pub(super) fn spoke_parked_for_delivery(state: &ServerState, run: &str) -> bool {
    state.spoke_turns.get(run) == Some(SpokeTurn::Parked)
}

impl ServerState {
    /// Note a turn boundary for a run no local slot tracks. Runs with a slot
    /// are covered by the slot's own activity, so they are not recorded here.
    pub(super) fn record_spoke_turn_event(&self, run: &str, event: &boss_protocol::WorkerEvent) {
        use boss_protocol::WorkerEvent;
        let turn = match event {
            WorkerEvent::Stop { .. } => SpokeTurn::Parked,
            WorkerEvent::UserPromptSubmit { .. }
            | WorkerEvent::PreToolUse { .. }
            | WorkerEvent::PostToolUse { .. }
            | WorkerEvent::ToolPoll { .. }
            | WorkerEvent::ModelProgress { .. } => SpokeTurn::Working,
            WorkerEvent::SessionStart { .. } | WorkerEvent::Notification { .. } | WorkerEvent::SessionEnd { .. } => {
                return;
            }
        };
        if self.worker_registry.slot_for_run(run).is_none() && self.spoke_probe_owner_remains(run) {
            self.spoke_turns.record(run, turn);
        }
    }

    /// A database read failure is not evidence that a remote worker vanished.
    pub(super) fn spoke_probe_owner_remains(&self, run: &str) -> bool {
        match self.work_db.spoke_assignment_for_execution(run) {
            Ok(assignment) => assignment.is_some_and(|assignment| !assignment.resolved),
            Err(error) => {
                tracing::warn!(run, error = %error, "cannot determine spoke ownership; retaining probe");
                true
            }
        }
    }
}
