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

/// Spoke ingress does not yet report worker activity, so mid-turn delivery
/// is allowed only when a local slot is mapped and already parked.
pub(super) fn spoke_parked_for_delivery(state: &ServerState, run: &str) -> bool {
    let Some(slot_id) = state.worker_registry.slot_for_run(run) else {
        return false;
    };
    state.pane_input_posture_for_run(run, slot_id) == super::pane_delivery::PaneInputPosture::Parked
}

impl ServerState {
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
