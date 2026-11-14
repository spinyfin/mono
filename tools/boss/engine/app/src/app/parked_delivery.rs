//! Verify that a parked pane submits the probe text before delivery counts.
//! A write retains the delivery slot until matching prompt or transcript
//! evidence confirms submission, or bounded Return retries fail visibly.
//! Retries send only Return; the text is written exactly once.

use super::*;

use boss_protocol::WorkerActivity;

use crate::app::pane_delivery::{PaneDeliveryDiagnostics, transcript_shows_text};

// Share the spawn path's measured submission bounds. The parked watch
// extends its deadline while busy, when sending Return is unsafe.
pub(super) use boss_execution_runner::spawn_confirmation::{
    SPAWN_CONFIRM_POLL as TURN_START_POLL, SUBMIT_CONFIRM_TIMEOUT as TURN_START_CONFIRM_TIMEOUT,
    SUBMIT_RETRIES as RETURN_RETRIES,
};

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
            if !server.probe_awaiting_turn_start(&write.run_id, &write.probe_id) {
                return;
            }
            if server.pane_typed_input_activity(write.slot_id) == Some(WorkerActivity::Working) {
                continue;
            }
            if returns_pressed == RETURN_RETRIES {
                fail_parked_write(&server, &write, returns_pressed, &diagnostics).await;
                return;
            }
            let pressed = server
                .resubmit_prompt_if(&write.run_id, || {
                    server.parked_probe_needs_return(&write.run_id, &write.probe_id, write.slot_id)
                })
                .await;
            if let Ok(false) = &pressed {
                continue;
            }
            returns_pressed += 1;
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
            // Losing the claim is not evidence that this text was submitted.
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
    let deferred_completion = server
        .parked_probe_snapshot(&write.run_id)
        .is_some_and(|claim| claim.probe_id == write.probe_id && claim.deferred_completion);
    if !server.settle_awaiting_probe(&write.run_id, &write.probe_id, false, detail.clone()) {
        return;
    }
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
    if deferred_completion {
        let outcome = server.completion_handler.on_stop(&write.run_id).await;
        tracing::info!(run_id = %write.run_id, ?outcome, "rechecked completion after parked delivery failed");
    }
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
    /// A main-session turn confirms only the text carried by this probe.
    /// Empty prompts and tool-only events need transcript evidence.
    pub(super) async fn confirm_parked_write_turn_started(&self, run_id: &str, prompt: Option<&str>) {
        let Some(claim) = self.parked_probe_snapshot(run_id) else {
            return;
        };
        let Some(text) = claim.awaiting_turn_text.as_deref() else {
            return;
        };
        let needle = super::pane_delivery::normalize_ws(text);
        let matches_prompt = !needle.is_empty()
            && prompt.is_some_and(|prompt| super::pane_delivery::normalize_ws(prompt).contains(&needle));
        let needs_transcript = prompt.is_none_or(|prompt| prompt.trim().is_empty());
        let matches_transcript = needs_transcript
            && !matches_prompt
            && if let Some(path) = claim.transcript_path.as_deref() {
                transcript_shows_text(path, claim.offset_bytes, text).await
            } else {
                false
            };
        if matches_prompt || matches_transcript {
            self.settle_awaiting_probe(
                run_id,
                &claim.probe_id,
                true,
                "written into the parked pane; matching submission evidence confirmed delivery".to_owned(),
            );
        }
    }

    fn settle_parked_write_delivered(&self, write: &ParkedWrite, evidence: &str) {
        self.settle_awaiting_probe(
            &write.run_id,
            &write.probe_id,
            true,
            format!("written into the parked pane; {evidence}"),
        );
    }
}

impl ServerState {
    /// Share identity validation with spawn retries; parked retries also
    /// recheck their claim after the last diagnostic or tmux await.
    pub(super) async fn resubmit_prompt_if(
        &self,
        execution_id: &str,
        permitted: impl Fn() -> bool,
    ) -> anyhow::Result<bool> {
        let identity = self
            .work_db
            .tmux_identity_for_execution(execution_id)?
            .ok_or_else(|| anyhow::anyhow!("no recorded tmux identity"))?;
        let tmux = self.tmux_for_pane_delivery(execution_id)?;
        let token = boss_engine_runtime::tmux_adoption::session_spawn_token(&tmux, &identity.session_name).await?;
        anyhow::ensure!(
            token.as_deref() == Some(identity.spawn_token.as_str()),
            "worker session token changed"
        );
        anyhow::ensure!(
            !self.work_db.get_execution(execution_id)?.status.is_terminal(),
            "execution is terminal"
        );
        self.leave_pane_modes_before_keys(execution_id, &tmux, &identity.session_name)
            .await?;
        if !permitted() {
            return Ok(false);
        }
        tmux.send_submit(&identity.session_name).await?;
        Ok(true)
    }
}
