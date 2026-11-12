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
/// Turn evidence reaches this map through `record_spoke_turn_event`, which the
/// hub's projection applier (`spoke_projection`) calls for each projected
/// main-session hook, durably and before that hook's worker-event fan-out. A
/// spoke host journals its worker's hook events and sends them to the hub as
/// stream records. Until a run's first turn event has been projected the map
/// has no entry for it and `spoke_parked_for_delivery` stays closed: mid-turn
/// delivery to a spoke worker is refused rather than guessed at.
#[derive(Default)]
pub(super) struct SpokeTurns(
    std::sync::Mutex<std::collections::HashMap<String, SpokeTurn>>,
    std::sync::atomic::AtomicBool,
);

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

    /// Publish a complete set of durable turn states. A run already recorded
    /// from a live event is newer than the stored row and is kept.
    fn restore_all(&self, restored: Vec<(String, bool)>) {
        if let Ok(mut turns) = self.0.lock() {
            for (run, parked) in restored {
                turns
                    .entry(run)
                    .or_insert(if parked { SpokeTurn::Parked } else { SpokeTurn::Working });
            }
        }
    }

    fn restored(&self) -> bool {
        self.1.load(std::sync::atomic::Ordering::Acquire)
    }

    fn mark_restored(&self) {
        self.1.store(true, std::sync::atomic::Ordering::Release);
    }

    #[cfg(test)]
    pub(super) fn clear(&self) {
        if let Ok(mut turns) = self.0.lock() {
            turns.clear();
        }
        self.1.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Mid-turn delivery to a spoke worker is allowed only once a turn's `Stop` has
/// been recorded and nothing since. A worker with no recorded turn boundary is
/// not known to be idle, so it is refused; as the type's docs explain, that is
/// a spoke worker whose first turn event has not been projected yet.
pub(super) fn spoke_parked_for_delivery(state: &ServerState, run: &str) -> bool {
    state.spoke_turns.get(run) == Some(SpokeTurn::Parked)
}

impl ServerState {
    /// Note a turn boundary for a spoke-supervised run. Local tmux slots keep
    /// their own activity, so they are skipped here. Subagent events never
    /// move the parent session's parked/working state.
    pub(super) fn record_spoke_turn_event(
        &self,
        run: &str,
        event: &boss_protocol::WorkerEvent,
        from_subagent: bool,
    ) -> Result<(), String> {
        use boss_protocol::WorkerEvent;
        if from_subagent {
            return Ok(());
        }
        if self
            .worker_registry
            .pane_for_run(run)
            .is_some_and(|pane| pane.tmux_session_name.is_some())
        {
            return Ok(());
        }
        let turn = match event {
            WorkerEvent::Stop { .. } => SpokeTurn::Parked,
            WorkerEvent::UserPromptSubmit { .. }
            | WorkerEvent::PreToolUse { .. }
            | WorkerEvent::PostToolUse { .. }
            | WorkerEvent::ToolPoll { .. }
            | WorkerEvent::ModelProgress { .. } => SpokeTurn::Working,
            WorkerEvent::SessionStart { .. } | WorkerEvent::Notification { .. } | WorkerEvent::SessionEnd { .. } => {
                return Ok(());
            }
        };
        // A remote worker gets a virtual live-status slot on its first hook.
        // That slot is display-only; mid-turn delivery still reads this map,
        // so turn state is keyed on spoke ownership rather than slot absence.
        if !self.spoke_probe_owner_remains(run) {
            return Ok(());
        }
        let assignment_generation = match self.work_db.spoke_assignment_for_execution(run) {
            Ok(Some(assignment)) => assignment.assignment_generation,
            Ok(None) => return Ok(()),
            Err(error) => return Err(format!("{error:#}")),
        };
        self.work_db
            .upsert_spoke_turn_state(&boss_work_db::SpokeTurnState {
                execution_id: run.to_owned(),
                assignment_generation,
                parked: turn == SpokeTurn::Parked,
            })
            .map_err(|error| format!("{error:#}"))?;
        self.spoke_turns.record(run, turn);
        Ok(())
    }

    /// Rebuild the in-memory turn map from durable rows after a hub restart.
    pub(super) fn restore_spoke_turns(&self) -> Result<(), String> {
        if self.spoke_turns.restored() {
            return Ok(());
        }
        let stored = self.work_db.spoke_turn_states().map_err(|error| format!("{error:#}"))?;
        // Build the whole set before publishing any of it, so a read failure
        // part-way never leaves a partially restored map that looks usable.
        let mut restored = Vec::with_capacity(stored.len());
        for turn in stored {
            match self.work_db.spoke_assignment_for_execution(&turn.execution_id) {
                Ok(Some(assignment))
                    if assignment.assignment_generation == turn.assignment_generation && !assignment.resolved =>
                {
                    restored.push((turn.execution_id, turn.parked));
                }
                Ok(_) => {}
                Err(error) => {
                    return Err(format!("{error:#}"));
                }
            }
        }
        self.spoke_turns.restore_all(restored);
        self.spoke_turns.mark_restored();
        Ok(())
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
