//! Confirming that a probe written into a parked pane started a turn.
//!
//! A probe for a worker sitting at its prompt is typed into the pane and
//! submitted with Return. tmux reporting success for those keys says they
//! were accepted, not that the driver took them as a prompt, and the two come
//! apart in ways that leave no trace at the write:
//!
//! * **The Return is taken as part of a paste.** Codex's TUI treats keys that
//!   arrive in a burst as a paste and a Return right behind them as a newline
//!   in it, so the text sits in the composer and nothing is submitted
//!   (measured: `docs/investigations/codex-0.160.1-qualification-2026-10-06.md`,
//!   "Harness notes"). Whether it happens depends on how long the Return's
//!   own `tmux` invocation takes to start, so it comes and goes with host
//!   load.
//! * **The pane is in a tmux mode.** A pane someone has scrolled is in
//!   copy-mode, where tmux consumes the keys itself: the text runs as
//!   copy-mode commands and the Return is swallowed.
//!
//! Both are prevented where the write is made ([`AgentDriver::pane_submit_settle`],
//! [`ServerState::leave_pane_modes_before_keys`]). This module is what makes
//! delivery not depend on having thought of every such cause: it holds the
//! probe at `Injected` until the driver is seen to start a turn, presses
//! Return again when it has not, and after a bounded number of tries settles
//! the delivery as failed, with what the pane looked like.
//!
//! **Why it matters beyond the one probe.** A probe written into a pane holds
//! the run's single delivery slot until the next turn boundary. A write that
//! starts no turn never reaches one, so the slot was held forever and every
//! later probe for the run queued behind it. The engine's own nudges were
//! among them, counted as they were queued: a worker was failed for ignoring
//! three nudges of which it had been shown none.
//!
//! The retry is Return and nothing else, the same rule the spawn path follows
//! for a brief that was pasted and not submitted
//! (`docs/worker-prompt-delivery.md`). The text is typed exactly once, so a
//! retry cannot deliver a second copy, and Return on an empty composer does
//! nothing.
//!
//! [`AgentDriver::pane_submit_settle`]: boss_engine_driver::AgentDriver::pane_submit_settle

use super::*;

use boss_engine_runtime::spawn_ack_sweep::SpawnAckReaper;
use boss_protocol::WorkerActivity;

use crate::app::pane_delivery::{PaneDeliveryDiagnostics, transcript_shows_text};

/// How long a write into a parked pane may go without the driver starting a
/// turn before the engine presses Return again.
///
/// The same bound, on the same measurement, as the spawn path's wait for a
/// submitted brief: Return to `UserPromptSubmit` took 37 to 198 ms at normal
/// load and up to 4.0 s under heavy load, so 30 s is 7.5 times the peak
/// (`docs/worker-prompt-delivery.md`). A turn that starts is seen in well
/// under a second; this only bounds how long one that does not is waited for.
pub(super) const TURN_START_CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// How many times Return is pressed again for a write that started no turn
/// before the delivery is settled as failed.
pub(super) const RETURN_RETRIES: u32 = 2;

/// How often the watch re-reads the run's state. The turn-start signal itself
/// is pushed ([`ServerState::confirm_parked_write_turn_started`]); the poll is
/// for the readings nothing pushes, and bounds how stale a release can be.
const TURN_START_POLL: Duration = Duration::from_millis(100);

/// The poll period while the pane is busy and the watch is only waiting for
/// it to come back to its prompt.
const BUSY_POLL: Duration = Duration::from_secs(1);

/// One write into a parked pane, as the watch needs it.
struct ParkedWrite {
    run_id: String,
    slot_id: u8,
    probe_id: String,
    /// The probe's own text, for finding it in the transcript.
    text: String,
}

/// Watch the write of `probe_id` into `run_id`'s parked pane until the driver
/// starts a turn on it or the delivery is settled as failed.
///
/// The caller has already marked the claim
/// ([`ServerState::mark_in_flight_probe_awaiting_turn_start`]). Detached: the
/// turn-start signal arrives through the same ingress that may be the caller,
/// so nothing on that path can wait for it.
pub(super) fn watch_parked_write(server: &Arc<ServerState>, run_id: &str, slot_id: u8, probe_id: &str, text: String) {
    boss_task_scope::spawn(confirm_turn_start(
        server.clone(),
        ParkedWrite {
            run_id: run_id.to_owned(),
            slot_id,
            probe_id: probe_id.to_owned(),
            text,
        },
    ));
}

/// Boxed with an explicit `Send` bound because a failed delivery hands the
/// run's queue back to [`dispatch_probe_now`], which can start another watch:
/// the cycle needs one named type in it.
fn confirm_turn_start(
    server: Arc<ServerState>,
    write: ParkedWrite,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        let mut returns_pressed = 0;
        loop {
            if wait_for_turn_start(&server, &write).await {
                return;
            }
            let diagnostics = server
                .capture_pane_delivery_diagnostics(&write.run_id, write.slot_id)
                .await;
            if returns_pressed == RETURN_RETRIES {
                fail_parked_write(&server, &write, returns_pressed, &diagnostics).await;
                return;
            }
            returns_pressed += 1;
            let pressed = SpawnAckReaper::resubmit_prompt(server.as_ref(), &write.run_id).await;
            tracing::warn!(
                run_id = %write.run_id,
                slot_id = write.slot_id,
                probe_id = %write.probe_id,
                attempt = returns_pressed,
                of = RETURN_RETRIES,
                waited = ?TURN_START_CONFIRM_TIMEOUT,
                error = ?pressed.err().map(|err| format!("{err:#}")),
                pane = %diagnostics.summary(),
                pane_tail = diagnostics.pane_tail.as_deref().unwrap_or(""),
                "probe written into a parked pane started no turn within the confirm window; pressed \
                 Return again",
            );
        }
    })
}

/// Wait up to [`TURN_START_CONFIRM_TIMEOUT`] of the pane sitting at its
/// prompt for the write to be settled one way or the other. `true` when there
/// is nothing left to watch; `false` when the window passed with the driver
/// still parked on the write.
///
/// A pane that reads `Working` is neither. The main session resuming is
/// pushed to the claim directly, so `Working` without that is a subagent, or
/// a turn whose start never reached the ingress; in both the next boundary
/// settles it, and a Return pressed into a busy pane can answer a dialog
/// nobody meant to answer. The window therefore only runs while the pane is
/// parked.
async fn wait_for_turn_start(server: &ServerState, write: &ParkedWrite) -> bool {
    let mut deadline = tokio::time::Instant::now() + TURN_START_CONFIRM_TIMEOUT;
    loop {
        // Anything but the two states a watched write can be in means another
        // path already settled it: the reply arrived, or teardown did.
        let state = server.probe_lifecycle_state(&write.probe_id);
        if !matches!(state, Some(ProbeDeliveryState::Injected | ProbeDeliveryState::Consumed)) {
            return true;
        }
        if server.worker_registry.slot_for_run(&write.run_id) != Some(write.slot_id) {
            // The pane was released. Teardown settles a written probe itself;
            // one it caught between the write and its record is left to the
            // task that owns the write, which is this one.
            settle_released_before_turn(server, write).await;
            return true;
        }
        if server.in_flight_probe_id(&write.run_id).as_deref() != Some(write.probe_id.as_str()) {
            // The claim is gone and the run is not. For a probe still
            // `Injected` only a turn boundary can have taken it. One already
            // recorded on the strength of its write may instead have been
            // displaced by a later probe, which is evidence of nothing.
            if state == Some(ProbeDeliveryState::Injected) {
                server.settle_parked_write_delivered(write, "a turn boundary followed the write");
            }
            return true;
        }
        if !server.probe_awaiting_turn_start(&write.run_id, &write.probe_id) {
            // The ingress saw the turn start and has recorded it.
            return true;
        }
        let busy = server.pane_typed_input_activity(write.slot_id) == Some(WorkerActivity::Working);
        let now = tokio::time::Instant::now();
        if busy {
            deadline = now + TURN_START_CONFIRM_TIMEOUT;
        } else if now >= deadline {
            break;
        }
        let poll = if busy { BUSY_POLL } else { TURN_START_POLL };
        tokio::time::sleep(poll.min(deadline.saturating_duration_since(now))).await;
    }
    // Last, for a driver whose turn start did not reach the ingress: the
    // transcript recording the text is the driver having taken it.
    let (transcript_path, offset_bytes) = server.in_flight_probe_snapshot(&write.run_id, &write.probe_id);
    if let Some(path) = transcript_path
        && transcript_shows_text(&path, offset_bytes, &write.text).await
    {
        server.take_probe_awaiting_turn_start(&write.run_id);
        server.settle_parked_write_delivered(write, "the transcript recorded the text");
        return true;
    }
    false
}

/// The run's pane was released with the write still unconfirmed.
async fn settle_released_before_turn(server: &ServerState, write: &ParkedWrite) {
    let cause = "the worker's pane was released before the driver started a turn on the written text";
    let settled = server.advance_probe_lifecycle(
        &write.probe_id,
        ProbeDeliveryState::Injected,
        ProbeDeliveryState::Orphaned,
        Some(format!("{cause}; re-issue against a live worker")),
    );
    if settled == Some(ProbeDeliveryState::Orphaned) {
        server
            .surface_undelivered_probe(&write.run_id, &write.probe_id, ProbeDeliveryState::Orphaned, cause)
            .await;
    }
}

/// The window passed [`RETURN_RETRIES`] + 1 times and no turn started.
///
/// Settles the probe `Unconfirmed`, which is the state for exactly this: the
/// bytes reached the pane and nothing shows the driver took them. Frees the
/// run's delivery slot, since no boundary is coming to do it, says so where
/// someone will see it, and hands the queue back to the dispatcher.
async fn fail_parked_write(
    server: &Arc<ServerState>,
    write: &ParkedWrite,
    returns_pressed: u32,
    diagnostics: &PaneDeliveryDiagnostics,
) {
    let waited_secs = TURN_START_CONFIRM_TIMEOUT.as_secs() * u64::from(returns_pressed + 1);
    let detail = format!(
        "written into the parked pane and Return pressed again {returns_pressed} times over \
         {waited_secs}s, but the driver never started a turn ({})",
        diagnostics.summary(),
    );
    let settled = [ProbeDeliveryState::Injected, ProbeDeliveryState::Consumed]
        .into_iter()
        .any(|from| {
            server.advance_probe_lifecycle(
                &write.probe_id,
                from,
                ProbeDeliveryState::Unconfirmed,
                Some(detail.clone()),
            ) == Some(ProbeDeliveryState::Unconfirmed)
        });
    if !settled {
        // Settled by another path between the last look and here.
        return;
    }
    server.set_probe_submission(&write.probe_id, true, Some(ProbeResumeEvidence::Unconfirmed));
    server.take_in_flight_probe_if(&write.run_id, &write.probe_id, |_| true);
    tracing::error!(
        run_id = %write.run_id,
        slot_id = write.slot_id,
        probe_id = %write.probe_id,
        returns_pressed,
        waited_secs,
        pane = %diagnostics.summary(),
        pane_tail = diagnostics.pane_tail.as_deref().unwrap_or(""),
        "probe delivery failed: the text was written into a parked pane and the driver never \
         started a turn on it; recorded unconfirmed, delivery slot released",
    );
    server
        .topic_broker
        .publish(
            &probe_topic(&write.run_id),
            FrontendEventEnvelope::push(FrontendEvent::ProbeDeliveryEscalated {
                run_id: write.run_id.clone(),
                probe_id: write.probe_id.clone(),
                reason: format!("probe settled unconfirmed: {detail}"),
            }),
        )
        .await;
    server
        .file_probe_attention(
            &write.run_id,
            &write.probe_id,
            format!("Probe {} was written but never taken by the worker", write.probe_id),
            unconfirmed_attention_body(write, &detail, diagnostics),
        )
        .await;
    // Whatever queued behind this probe was waiting on a boundary that is
    // not coming either.
    if server.has_pending_probe(&write.run_id) {
        let outcome = dispatch_probe_now(server, &write.run_id).await;
        tracing::info!(
            run_id = %write.run_id,
            outcome = outcome.as_str(),
            "delivery slot released after a failed parked write; attempted the next queued probe",
        );
    }
}

fn unconfirmed_attention_body(write: &ParkedWrite, detail: &str, diagnostics: &PaneDeliveryDiagnostics) -> String {
    let pane_tail = match diagnostics.pane_tail.as_deref() {
        Some(tail) => format!("```\n{tail}\n```"),
        None => "(the pane could not be read, or was blank)".to_owned(),
    };
    format!(
        "A probe for this run was typed into the worker's pane and the worker never started a \
         turn on it.\n\n\
         - probe: `{probe_id}`\n\
         - execution: `{run_id}`\n\
         - final delivery state: `unconfirmed`\n\n\
         What happened: {detail}.\n\n\
         The text reached the pane, so it may still be sitting in the worker's composer. \
         Re-issuing the same instruction can therefore reach the worker twice; look at the pane \
         first. If the instruction was load-bearing, it has not taken effect.\n\n\
         The pane when the engine gave up:\n\n{pane_tail}\n\n\
         Full record: `bossctl probe-status {probe_id}`.",
        probe_id = write.probe_id,
        run_id = write.run_id,
    )
}

impl ServerState {
    /// The main session of `run_id` started a turn. If a probe was written
    /// into its parked pane and is waiting on exactly that, the probe was
    /// taken.
    ///
    /// Called from the ingress for every turn start. A no-op for all but the
    /// one a parked write produced.
    pub(super) fn confirm_parked_write_turn_started(&self, run_id: &str) {
        let Some(probe_id) = self.take_probe_awaiting_turn_start(run_id) else {
            return;
        };
        let state = self.advance_probe_lifecycle(
            &probe_id,
            ProbeDeliveryState::Injected,
            ProbeDeliveryState::Consumed,
            Some("written into the parked pane; the driver started a turn on it".to_owned()),
        );
        self.set_probe_submission(&probe_id, true, Some(ProbeResumeEvidence::Confirmed));
        tracing::info!(
            run_id,
            probe_id,
            state = state.map(ProbeDeliveryState::as_str),
            "probe delivery confirmed: the driver started a turn after the write into its parked pane",
        );
    }

    /// Record evidence, other than the turn-start signal itself, that the
    /// driver took a parked write.
    fn settle_parked_write_delivered(&self, write: &ParkedWrite, evidence: &str) {
        let state = self.advance_probe_lifecycle(
            &write.probe_id,
            ProbeDeliveryState::Injected,
            ProbeDeliveryState::Consumed,
            Some(format!("written into the parked pane; {evidence}")),
        );
        self.set_probe_submission(&write.probe_id, true, Some(ProbeResumeEvidence::Confirmed));
        tracing::info!(
            run_id = %write.run_id,
            probe_id = %write.probe_id,
            evidence,
            state = state.map(ProbeDeliveryState::as_str),
            "probe delivery confirmed for a write into a parked pane",
        );
    }
}
