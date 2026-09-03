//! Interrupting probe delivery: cut the worker's turn short, prove it
//! stopped, *then* type.
//!
//! The boundary-only delivery paths in [`super::worker_events`] all wait for
//! something the worker chooses to do — finish a tool call (`PostToolUse`),
//! finish a turn (`Stop`), or sit at its prompt. For the case a probe
//! actually exists to serve, that is the wrong shape: a worker mid-way
//! through a long autonomous run reaches its *next* turn boundary and is torn
//! down in the same breath, so an instruction meant to redirect it is read by
//! nobody. This module makes the boundary happen instead of waiting for one.
//!
//! **The sequence is the point, and every step of it is load-bearing:**
//!
//! 1. **Claim first.** The probe and the run's single in-flight slot are
//!    reserved *before* any keystroke goes out. Interrupting produces a turn
//!    boundary, and a turn boundary is exactly what wakes the other
//!    dispatchers — without the claim, `dispatch_probe_on_stop` would race in
//!    on the interrupt's own `Stop` and deliver the same probe, so the worker
//!    could receive it twice or receive it without the interrupted-notice.
//! 2. **Skip the interrupt when there is nothing to interrupt.** A parked
//!    worker gets the text written straight in.
//!    ([`boss_protocol::ProbeInterruptOutcome::NotNeeded`].) Interrupting
//!    costs the worker whatever it was doing; spending that on an idle worker
//!    buys nothing.
//! 3. **Interrupt per the driver's own plan.**
//!    [`crate::driver::AgentDriver::interrupt_plan`] says which key, how many
//!    presses, how long to wait and how many attempts. Nothing here knows
//!    which agent it is driving.
//! 4. **Confirm the turn actually ended before typing.** This is the step
//!    whose absence corrupts input: text written into a still-running turn is
//!    swallowed, re-wrapped, or read as an answer to a permission prompt.
//!    Confirmation is the engine's own turn-boundary signal (the slot's live
//!    [`boss_protocol::WorkerActivity`] leaving `Working`, fed by the same
//!    hook/rollout/recovery ingress every other boundary uses), with the
//!    driver's pane-monitor busy markers as a second, independent read of the
//!    same fact. Not a sleep.
//! 5. **Retry bounded, then fail loudly.** The driver's plan caps the
//!    attempts. When they are spent, the probe settles
//!    [`ProbeDeliveryState::InterruptFailed`] — terminal, visibly failed,
//!    surfaced as an attention item — and **nothing is typed**. A probe that
//!    could not be delivered must never look like one that was.
//!
//! **Failure here writes nothing.** That is what distinguishes
//! `InterruptFailed` from [`ProbeDeliveryState::Orphaned`]: no bytes reached
//! the pty, so no late reply can arrive to correct the record, and re-issuing
//! the instruction cannot duplicate one that quietly landed.
//!
//! The write itself is [`ServerState::inject_pane_text_verified`] — the same
//! verified path the chore-update notice and the mid-turn probe use, which
//! reaches `Tmux::send_keys` and therefore the bracketed-paste/chunking that
//! keeps long multi-line text from being truncated at the tty's canonical
//! line-length limit. This module deliberately owns no second way to write to
//! a pty.

use super::*;

use boss_protocol::{ProbeInterruptOutcome, WorkerActivity};

use crate::app::pane_delivery::{PaneInjectOutcome, PaneInjectRequest, PaneInputPosture};
use crate::app::probes::PendingProbe;
use crate::driver::InterruptPlan;

/// How often the confirmation wait re-reads the slot's live activity.
///
/// Small relative to any driver's `confirm_window` so a turn that ends
/// promptly is acted on promptly — the whole point of interrupting is that
/// the text lands *now*, and idling out a fixed window after the turn already
/// stopped would give that back.
const ACTIVITY_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// How often the confirmation wait falls back to reading the pane's own text.
///
/// An order of magnitude slower than [`ACTIVITY_POLL_INTERVAL`] because each
/// read spawns a `tmux capture-pane`. This is a *secondary* signal: the
/// primary one (live activity) is driven by the driver's own turn-boundary
/// ingress, and this exists so a degraded ingress does not make every
/// interrupting probe fail on a worker that visibly stopped.
const PANE_TEXT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long the post-interrupt write waits for a consumption-confirming
/// signal (`UserPromptSubmit` or transcript) before falling back to the
/// parked-write contract. A pane capture is only an echo of our write and
/// still takes that liveness-gated contract; a successful `send_keys` into a
/// pane whose turn we already proved ended, with the process still alive, is
/// `Consumed` — the same evidence the Stop-boundary parked path records
/// without waiting at all. The window is for the stronger signals to win
/// when they are fast, not a bound on whether the write counted.
const POST_INTERRUPT_VERIFY_TIMEOUT: Duration = Duration::from_secs(6);

/// How long to wait for another dispatcher that already claimed this probe
/// (or the run's single in-flight slot) to record an outcome, before
/// reporting whatever state it has so far. Sized just past the mid-turn
/// verification window so a `PostToolUse` path that won the race can finish
/// its own confirm rather than being reported as `queued` mid-write.
const OTHER_PATH_SETTLE_TIMEOUT: Duration = Duration::from_secs(8);

/// Prefix on text delivered after a turn was actually cut short.
///
/// An interrupt aborts whatever the worker was mid-way through — an edit that
/// wrote half a file, a `bazel` invocation killed between actions, a lock it
/// took and had not released. The worker's own model of "what I just did"
/// stops being true at that instant, and it has no other way to find out:
/// from inside the session an interrupted tool call and a completed one look
/// alike. Telling it explicitly is what lets it reconcile rather than build
/// on an action that never finished.
pub(super) const INTERRUPT_NOTICE: &str = "[coordinator-interrupt] The coordinator cut your current turn \
     short to deliver this message now. Whatever you were doing at that moment was aborted mid-flight — a \
     file edit may be half-applied, a command may have been killed, a build may have been abandoned. Verify \
     the state of your last action before continuing; do not assume it completed.";

/// Prefix used when no interrupt was needed (the worker was already parked)
/// or none was sent (it reached a boundary on its own first). Same marking
/// the other probe paths use, so the transcript reads consistently and the
/// worker is not told its work was discarded when it was not.
const NUDGE_NOTICE: &str = "[coordinator-nudge]";

/// Everything the interrupting delivery path settled, for the caller to
/// report back over the wire.
///
/// Carries the interrupt outcome separately from the delivery state because
/// they answer different questions and the operator needs both: `state` says
/// whether the worker got the text, `interrupt` says what it cost to get it
/// there.
#[derive(Debug)]
pub(super) struct InterruptingDelivery {
    pub(super) state: ProbeDeliveryState,
    pub(super) interrupt: ProbeInterruptOutcome,
    pub(super) attempts: u8,
    pub(super) detail: Option<String>,
}

impl InterruptingDelivery {
    fn failed(interrupt: ProbeInterruptOutcome, attempts: u8, detail: String) -> Self {
        Self {
            state: ProbeDeliveryState::InterruptFailed,
            interrupt,
            attempts,
            detail: Some(detail),
        }
    }
}

/// Why a confirmation wait stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnEndWait {
    /// The worker is parked: the turn ended and the pane will take a write.
    Ended,
    /// The confirm window elapsed with the worker still mid-turn.
    StillWorking,
    /// The run went away while we waited — its slot mapping was cleared, or
    /// its activity went terminal. There is nothing left to type into.
    WorkerGone,
}

/// Deliver `probe_id` (already queued for `run_id`) by interrupting the
/// worker's in-flight turn first.
///
/// Runs synchronously inside the `ProbeRun` RPC — the caller answers with the
/// settled state rather than an intention, which is the whole reason this
/// path exists. Bounded by the driver's plan
/// ([`InterruptPlan::worst_case_duration`]) plus one verification window, so
/// it cannot hang the request indefinitely.
pub(super) async fn deliver_probe_interrupting(
    server_state: &Arc<ServerState>,
    run_id: &str,
    probe_id: &str,
) -> InterruptingDelivery {
    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        return abandon_before_write(
            server_state,
            run_id,
            probe_id,
            "the run's pane was released between the probe being queued and delivery starting",
        )
        .await;
    };

    // Claim before interrupting — see the module docs. From here every exit
    // must either complete the write, requeue the claim, or settle the probe
    // terminally; a claim left behind would pin the run's single delivery
    // slot against a probe nothing is going to deliver.
    let (transcript_path, offset_bytes) = super::worker_events::transcript_offset_for_run(server_state, run_id).await;
    let Some(probe) =
        server_state.try_reserve_specific_probe_for_delivery(run_id, probe_id, transcript_path, offset_bytes)
    else {
        // Either another dispatch path holds the run's delivery slot, or this
        // exact probe id is not present in the queue at all (already claimed
        // by another dispatcher, or already settled). Wait for that path to
        // record an outcome when it is delivering *this* probe, rather than
        // reporting `queued` for a write that is already in flight.
        return raced_to_another_dispatcher(server_state, run_id, probe_id).await;
    };

    let (interrupt, attempts) = match interrupt_and_confirm(server_state, run_id, slot_id).await {
        Ok(result) => result,
        Err(failure) => return settle_interrupt_failure(server_state, run_id, probe, failure).await,
    };

    inject_after_interrupt(server_state, run_id, slot_id, probe, interrupt, attempts).await
}

/// How the interrupt phase failed, before any text was written.
struct InterruptFailure {
    outcome: ProbeInterruptOutcome,
    attempts: u8,
    /// Operator-facing explanation, recorded on the probe and surfaced as an
    /// attention item.
    detail: String,
    /// True when the failure is the run having gone away rather than the
    /// interrupt not taking — those settle at different states, because an
    /// abandoned run is nobody's fault and a wedged interrupt is a capability
    /// gap worth an operator's attention.
    worker_gone: bool,
}

/// Fail before any keystroke if the subsequent pane write cannot land.
fn refuse_if_write_transport_not_ready(server_state: &ServerState, run_id: &str) -> Result<(), InterruptFailure> {
    server_state.pane_write_transport_ready(run_id).map_err(|detail| {
        tracing::warn!(
            run_id,
            %detail,
            "refusing to interrupt: pane write transport is not ready",
        );
        InterruptFailure {
            outcome: ProbeInterruptOutcome::Failed,
            attempts: 0,
            detail: format!("the pane write cannot land ({detail}); the worker's turn was not interrupted"),
            worker_gone: false,
        }
    })
}

/// Bring the worker to a parked posture, interrupting if it is mid-turn.
///
/// `Ok((outcome, attempts))` means the pane will now take a write.
async fn interrupt_and_confirm(
    server_state: &Arc<ServerState>,
    run_id: &str,
    slot_id: u8,
) -> Result<(ProbeInterruptOutcome, u8), InterruptFailure> {
    match server_state.pane_typed_input_activity(slot_id) {
        // Already parked: nothing to interrupt, and interrupting anyway would
        // cost the worker a turn for no reason. On Claude Code it would cost
        // more than nothing — a second Escape at the prompt opens the rewind
        // picker, a modal that would swallow the text we are about to type.
        Some(activity) if activity.accepts_typed_input() => {
            refuse_if_write_transport_not_ready(server_state, run_id)?;
            return Ok((ProbeInterruptOutcome::NotNeeded, 0));
        }
        Some(WorkerActivity::Working) => {}
        // Pre-session or terminal. There is no turn to interrupt and no pane
        // that will take a write; the caller routes these away before we get
        // here, so reaching this is a race with teardown.
        other => {
            return Err(InterruptFailure {
                outcome: ProbeInterruptOutcome::WorkerGone,
                attempts: 0,
                detail: format!(
                    "the worker's activity is {} — there is no in-flight turn to interrupt and no \
                     prompt to write to",
                    other.map(WorkerActivity::as_str).unwrap_or("unknown"),
                ),
                worker_gone: true,
            });
        }
    }

    let plan = match server_state.interrupt_plan_lookup(run_id) {
        super::pane_ops::InterruptPlanLookup::Plan(plan) => plan,
        // A declared property of the driver, not a silent fallback: we do not
        // quietly downgrade to boundary delivery and report success.
        super::pane_ops::InterruptPlanLookup::NotInterruptible => {
            return Err(InterruptFailure {
                outcome: ProbeInterruptOutcome::DriverCannotInterrupt,
                attempts: 0,
                detail: format!(
                    "the driver for run {run_id} declares no interrupt mechanism, so its in-flight turn \
                     cannot be cut short; nothing was written into the pane"
                ),
                worker_gone: false,
            });
        }
        // Distinct from the above: no driver resolved at all (missing
        // execution row, unregistered slug, DB error) rather than a driver
        // that was resolved and genuinely declares itself uninterruptible.
        // Every registered driver declares a runnable plan, so this is the
        // case that actually occurs in practice, and it is a lookup failure
        // an operator can fix, not a permanent capability gap.
        super::pane_ops::InterruptPlanLookup::DriverUnresolved => {
            return Err(InterruptFailure {
                outcome: ProbeInterruptOutcome::DriverCannotInterrupt,
                attempts: 0,
                detail: format!(
                    "the engine could not resolve a driver for run {run_id}, so no interrupt gesture \
                     could be chosen; nothing was written into the pane"
                ),
                worker_gone: false,
            });
        }
    };

    // Confirm the write can land BEFORE sending Escape. Interrupt uses the
    // in-memory session name and can succeed while the subsequent write
    // fails with `no durable tmux identity recorded for run`. That sequence
    // cuts a healthy worker's turn and delivers nothing.
    refuse_if_write_transport_not_ready(server_state, run_id)?;

    let mut attempts: u8 = 0;
    for attempt in 1..=plan.max_attempts.max(1) {
        // Re-read immediately before each gesture: the worker may have
        // reached a boundary on its own while we were preparing, or between
        // attempts. Sending Escape into a pane that is already at its prompt
        // is not harmless (see the rewind-picker note above), so this check
        // is a guard, not an optimisation.
        match server_state.pane_typed_input_activity(slot_id) {
            Some(activity) if activity.accepts_typed_input() => {
                let outcome = if attempts == 0 {
                    ProbeInterruptOutcome::EndedNaturally
                } else {
                    // We did send a gesture; whether this boundary is ours or
                    // the worker's own, the interrupt is what we account for.
                    ProbeInterruptOutcome::Interrupted
                };
                return Ok((outcome, attempts));
            }
            Some(WorkerActivity::Working) => {}
            other => {
                return Err(InterruptFailure {
                    outcome: ProbeInterruptOutcome::WorkerGone,
                    attempts,
                    detail: format!(
                        "the worker went {} while its turn was being interrupted",
                        other.map(WorkerActivity::as_str).unwrap_or("untracked"),
                    ),
                    worker_gone: true,
                });
            }
        }

        if let Err(err) = server_state.deliver_interrupt_gesture(run_id, Some(&plan)).await {
            let worker_gone = matches!(err, super::pane_ops::InterruptPaneError::UnknownRun);
            return Err(InterruptFailure {
                outcome: if worker_gone {
                    ProbeInterruptOutcome::WorkerGone
                } else {
                    ProbeInterruptOutcome::Failed
                },
                attempts,
                detail: format!("the interrupt keystroke could not be delivered to the worker's pane: {err}"),
                worker_gone,
            });
        }
        attempts = attempt;
        tracing::info!(
            run_id,
            slot_id,
            attempt,
            max_attempts = plan.max_attempts,
            key = plan.gesture.key,
            presses = plan.gesture.presses,
            // The bound this whole phase is held to, so an operator reading
            // the log knows how long the synchronous `ProbeRun` can take
            // before it answers rather than having to derive it.
            budget = ?plan.worst_case_duration(),
            "interrupt delivered for probe delivery; waiting for the turn to actually end",
        );

        match wait_for_turn_end(server_state, run_id, slot_id, &plan).await {
            TurnEndWait::Ended => return Ok((ProbeInterruptOutcome::Interrupted, attempts)),
            TurnEndWait::WorkerGone => {
                return Err(InterruptFailure {
                    outcome: ProbeInterruptOutcome::WorkerGone,
                    attempts,
                    detail: "the run's pane was released while its turn was being interrupted".to_owned(),
                    worker_gone: true,
                });
            }
            TurnEndWait::StillWorking => {
                // The next attempt re-arms this driver's recovery observer if
                // it has one, so a driver in that class can have two in
                // flight at once. Harmless by construction: each one either
                // observes the same cancellation record or synthesizes a turn
                // end, and a synthesized turn end for a slot that is already
                // parked is a no-op. Not worth de-duplicating at the cost of
                // threading observer identity through the interrupt path.
                tracing::warn!(
                    run_id,
                    slot_id,
                    attempt,
                    confirm_window = ?plan.confirm_window,
                    "interrupt did not take within the driver's confirm window",
                );
            }
        }
    }

    Err(InterruptFailure {
        outcome: ProbeInterruptOutcome::Failed,
        attempts,
        detail: format!(
            "the worker's turn did not end after {attempts} interrupt attempt(s) of {} x {:?} with a \
             {:?} confirm window each; nothing was written into the pane, because typing into a \
             still-running turn is how injected text gets swallowed or misread",
            plan.gesture.presses, plan.gesture.key, plan.confirm_window,
        ),
        worker_gone: false,
    })
}

/// Wait up to `plan.confirm_window` for evidence that the interrupted turn
/// ended.
///
/// Two independent reads of the same fact:
///
/// * the slot's live [`WorkerActivity`], which is the engine's ordinary
///   turn-boundary signal — fed by Claude's `Stop` hook, Codex's rollout
///   `turn_aborted`, and (for a driver whose cancelled turn skips its
///   boundary channel entirely) the interrupt-recovery observer armed by
///   [`ServerState::deliver_interrupt_gesture`]; and
/// * the pane's own text, matched against the driver's
///   [`boss_protocol::PaneMonitorSpec`] busy markers — the same literals the
///   app's pane monitor screen-scrapes. Slower and secondary, so a degraded
///   event ingress does not make every interrupt look wedged on a worker that
///   visibly stopped.
///
/// Never a fixed sleep: an interrupt that takes 200 ms is acted on in 200 ms.
async fn wait_for_turn_end(
    server_state: &Arc<ServerState>,
    run_id: &str,
    slot_id: u8,
    plan: &InterruptPlan,
) -> TurnEndWait {
    let deadline = tokio::time::Instant::now() + plan.confirm_window;
    let mut next_pane_read = tokio::time::Instant::now() + PANE_TEXT_POLL_INTERVAL;
    loop {
        match server_state.pane_typed_input_activity(slot_id) {
            Some(activity) if activity.accepts_typed_input() => return TurnEndWait::Ended,
            Some(WorkerActivity::Working) => {}
            // No live state, or the worker went terminal: nothing to type
            // into. `Spawning` cannot follow `Working` for a live run, so it
            // lands here too rather than being treated as a turn end.
            _ => return TurnEndWait::WorkerGone,
        }
        if server_state.worker_registry.slot_for_run(run_id) != Some(slot_id) {
            return TurnEndWait::WorkerGone;
        }
        let now = tokio::time::Instant::now();
        if now >= next_pane_read {
            next_pane_read = now + PANE_TEXT_POLL_INTERVAL;
            if server_state.pane_text_shows_turn_ended(run_id).await {
                tracing::info!(
                    run_id,
                    slot_id,
                    "interrupt confirmed by the pane's own text (no busy marker) before the live \
                     activity signal caught up",
                );
                return TurnEndWait::Ended;
            }
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return TurnEndWait::StillWorking;
        }
        tokio::time::sleep(ACTIVITY_POLL_INTERVAL.min(deadline.saturating_duration_since(now))).await;
    }
}

/// Write the probe into the now-parked pane and record what became of it.
///
/// The claim is already held, so this is the ordinary verified-write path
/// with two differences: the text carries [`INTERRUPT_NOTICE`] when work was
/// actually discarded, and the in-flight entry's transcript offset is
/// re-snapshotted first so the interrupted turn's own trailing output is not
/// later mistaken for the worker's reply.
async fn inject_after_interrupt(
    server_state: &Arc<ServerState>,
    run_id: &str,
    slot_id: u8,
    probe: PendingProbe,
    interrupt: ProbeInterruptOutcome,
    attempts: u8,
) -> InterruptingDelivery {
    let probe_id = probe.probe_id.clone();
    let marker = if interrupt.discarded_in_flight_work() {
        INTERRUPT_NOTICE
    } else {
        NUDGE_NOTICE
    };
    let marked_text = format!("{marker}\n\n{}", probe.text);

    // Re-snapshot after the turn ended: the reply we want is the worker's
    // answer to *this* text, not the tail of the turn we just cancelled.
    let (transcript_path, offset_bytes) = super::worker_events::transcript_offset_for_run(server_state, run_id).await;
    server_state.update_in_flight_probe_snapshot(run_id, transcript_path.clone(), offset_bytes);

    let mut posture = server_state.pane_input_posture_for_run(run_id, slot_id);
    if !posture.permits_write() && server_state.pane_text_shows_turn_ended(run_id).await {
        // The live activity signal has not caught up yet (still `Working`,
        // so `pane_input_posture_for_run` resolves `Refused` on a driver that
        // rejects mid-turn input), but the pane's own text positively shows
        // the worker parked — a prompt prefix present and no busy marker,
        // exactly the fact `pane_text_shows_turn_ended` exists to confirm.
        // Without this, the secondary confirmation signal could never
        // actually complete a delivery on the driver class it exists for
        // (one whose cancelled turn skips its own turn-boundary channel): it
        // would confirm the turn ended, then immediately have the write
        // guard refuse anyway, discarding the interrupted work for nothing.
        posture = PaneInputPosture::Parked;
    }
    if !posture.permits_write() {
        // The worker started another turn (or went away) between the
        // confirmation and this write. Give the claim back rather than
        // writing into a posture the guard refuses.
        return requeue_after_write_declined(
            server_state,
            run_id,
            probe,
            interrupt,
            attempts,
            "the worker left its prompt again between the interrupt being confirmed and the text \
             being written",
        )
        .await;
    }

    server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Injected);
    let outcome = server_state
        .inject_pane_text_verified(
            PaneInjectRequest::builder()
                .run_id(run_id)
                .slot_id(slot_id)
                .text(marked_text)
                .maybe_transcript_path(transcript_path.as_deref())
                .offset_bytes(offset_bytes)
                .verify_timeout(POST_INTERRUPT_VERIFY_TIMEOUT)
                .posture(posture)
                .build(),
        )
        .await;

    match outcome {
        PaneInjectOutcome::Confirmed => {
            tracing::info!(
                run_id,
                slot_id,
                probe_id = %probe_id,
                interrupt = interrupt.as_str(),
                attempts,
                "probe delivered after interrupting the worker's turn (delivery confirmed)",
            );
            server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Consumed);
            InterruptingDelivery {
                state: ProbeDeliveryState::Consumed,
                interrupt,
                attempts,
                detail: None,
            }
        }
        PaneInjectOutcome::PaneEcho => {
            let state = super::worker_events::record_pane_write_outcome(
                server_state,
                run_id,
                slot_id,
                &probe_id,
                ProbeDeliveryState::Consumed,
                "probe pane echo observed after interrupt (parked-write consumption)",
            )
            .await;
            InterruptingDelivery {
                state,
                interrupt,
                attempts,
                detail: None,
            }
        }
        // A parked worker submitting nothing is not the mid-turn `Buffered`
        // shape — it is the same "wrote it, could not prove it" the parked
        // path already records as `Unconfirmed`. `inject_pane_text_verified`
        // only returns `Buffered` for a mid-turn posture, so reaching it here
        // means the posture flipped under us mid-write; record it honestly
        // rather than claiming consumption.
        PaneInjectOutcome::Buffered => {
            tracing::info!(
                run_id,
                slot_id,
                probe_id = %probe_id,
                "probe written after an interrupt but the worker was mid-turn again by then; \
                 recorded buffered",
            );
            server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Buffered);
            InterruptingDelivery {
                state: ProbeDeliveryState::Buffered,
                interrupt,
                attempts,
                detail: None,
            }
        }
        PaneInjectOutcome::Unconfirmed => {
            // The worker is parked by construction — we confirmed the turn
            // ended before typing. A successful write into a parked pane is
            // the same evidence `deliver_probe_via_pane_write` records as
            // Consumed without waiting for a hook. UserPromptSubmit for
            // pane-injected text has never been validated end-to-end and
            // routinely misses this window (Grok's hooks block the TUI for
            // up to 15s after an interrupted tool call; JSONL stores
            // multi-line injections escaped). Treating that miss as "NOT
            // delivered" is the false negative this path existed to close.
            // Liveness still gates the claim: a dead process is Orphaned.
            tracing::info!(
                run_id,
                slot_id,
                probe_id = %probe_id,
                timeout = ?POST_INTERRUPT_VERIFY_TIMEOUT,
                "probe written into a parked pane after interrupt; UserPromptSubmit/transcript did \
                 not confirm within the window — recording parked-write consumption",
            );
            let state = super::worker_events::record_pane_write_outcome(
                server_state,
                run_id,
                slot_id,
                &probe_id,
                ProbeDeliveryState::Consumed,
                "probe written into parked pane after interrupt (parked-write consumption)",
            )
            .await;
            InterruptingDelivery {
                state,
                interrupt,
                attempts,
                detail: None,
            }
        }
        PaneInjectOutcome::NotAcceptingInput { activity } => {
            requeue_after_write_declined(
                server_state,
                run_id,
                probe,
                interrupt,
                attempts,
                &format!(
                    "the pane guard refused the write after the interrupt was confirmed (activity={})",
                    activity.map(WorkerActivity::as_str).unwrap_or("unknown"),
                ),
            )
            .await
        }
        PaneInjectOutcome::SendFailed(failure) => {
            tracing::warn!(
                ?failure,
                run_id,
                slot_id,
                probe_id = %probe_id,
                "post-interrupt probe injection failed at the transport",
            );
            requeue_after_write_declined(
                server_state,
                run_id,
                probe,
                interrupt,
                attempts,
                "the pane write failed at the transport after the interrupt was confirmed",
            )
            .await
        }
    }
}

/// The interrupt worked but the write did not happen. Give the claim back so
/// a later boundary can retry with the same probe id, or settle `Abandoned`
/// when the run is already gone and no boundary is coming.
///
/// Deliberately **not** `InterruptFailed`: the interrupt did take. Reporting
/// otherwise would send an operator to fix the wrong thing.
async fn requeue_after_write_declined(
    server_state: &Arc<ServerState>,
    run_id: &str,
    probe: PendingProbe,
    interrupt: ProbeInterruptOutcome,
    attempts: u8,
    reason: &str,
) -> InterruptingDelivery {
    let probe_id = probe.probe_id.clone();
    server_state.completion_handler.revert_undelivered_nudge(run_id);
    server_state.set_probe_lifecycle(&probe_id, ProbeDeliveryState::Queued);
    if server_state.release_probe_reservation(run_id, probe) {
        tracing::warn!(
            run_id,
            probe_id = %probe_id,
            reason,
            "interrupting probe delivery could not write; re-queued for a later boundary",
        );
        InterruptingDelivery {
            state: ProbeDeliveryState::Queued,
            interrupt,
            attempts,
            detail: Some(format!(
                "{reason}; the probe was re-queued for the worker's next boundary"
            )),
        }
    } else {
        let detail = format!("{reason}, and the run had already been released, so there was nothing to retry against");
        server_state
            .surface_undelivered_probe(run_id, &probe_id, ProbeDeliveryState::Abandoned, &detail)
            .await;
        InterruptingDelivery {
            state: ProbeDeliveryState::Abandoned,
            interrupt,
            attempts,
            detail: Some(detail),
        }
    }
}

/// Settle a probe whose interrupt phase failed. Nothing was written, so this
/// is terminal: no reply can arrive to correct it.
async fn settle_interrupt_failure(
    server_state: &Arc<ServerState>,
    run_id: &str,
    probe: PendingProbe,
    failure: InterruptFailure,
) -> InterruptingDelivery {
    let probe_id = probe.probe_id.clone();
    // Drop the claim without re-queuing: the probe is settled terminally, and
    // leaving it in the pending queue would have a later boundary deliver
    // text the caller has already been told did not arrive.
    server_state.take_in_flight_probe(run_id);
    let state = if failure.worker_gone {
        ProbeDeliveryState::Abandoned
    } else {
        ProbeDeliveryState::InterruptFailed
    };
    tracing::warn!(
        run_id,
        probe_id = %probe_id,
        interrupt = failure.outcome.as_str(),
        attempts = failure.attempts,
        state = state.as_str(),
        detail = %failure.detail,
        "interrupting probe delivery failed before anything was written into the pane",
    );
    server_state.set_probe_lifecycle_detail(&probe_id, state, Some(failure.detail.clone()));
    server_state
        .surface_undelivered_probe(run_id, &probe_id, state, &failure.detail)
        .await;
    server_state.completion_handler.revert_undelivered_nudge(run_id);
    InterruptingDelivery::failed(failure.outcome, failure.attempts, failure.detail)
}

/// The run's pane was gone before delivery even started.
///
/// Drains and surfaces the run's **whole** queue, not just this probe: the
/// pane is gone, so every probe still waiting on it is equally undeliverable,
/// and settling only the one the caller asked about would leave its siblings
/// reading `queued` forever with nothing left to deliver them.
async fn abandon_before_write(
    server_state: &Arc<ServerState>,
    run_id: &str,
    probe_id: &str,
    cause: &str,
) -> InterruptingDelivery {
    let drained = server_state.abandon_pending_probes_for_terminated_run(run_id, cause);
    server_state
        .surface_undelivered_probes(run_id, &drained, ProbeDeliveryState::Abandoned, cause)
        .await;
    if !drained.iter().any(|id| id == probe_id) {
        // The caller's own probe was not in the queue (already claimed and
        // settled elsewhere, or drained by a concurrent teardown). Surface it
        // explicitly so the answer we are about to give is backed by a record.
        server_state
            .surface_undelivered_probe(run_id, probe_id, ProbeDeliveryState::Abandoned, cause)
            .await;
    }
    InterruptingDelivery {
        state: ProbeDeliveryState::Abandoned,
        interrupt: ProbeInterruptOutcome::WorkerGone,
        attempts: 0,
        detail: Some(cause.to_owned()),
    }
}

/// Another dispatch path claimed the probe (or the run's delivery slot)
/// first. Report the probe's own recorded state rather than inventing one.
///
/// When that other path is in the middle of delivering *this* probe (the
/// in-flight slot names `probe_id` and the lifecycle is still `Queued` /
/// `Injected`), wait for it to record an outcome so the interrupting RPC
/// does not answer `queued` for a write that is already in flight — the
/// shape that made a delivered probe look like a lost one.
async fn raced_to_another_dispatcher(
    server_state: &Arc<ServerState>,
    run_id: &str,
    probe_id: &str,
) -> InterruptingDelivery {
    let in_flight_id = server_state.in_flight_probe_id(run_id);
    let state = if in_flight_id.as_deref() == Some(probe_id) {
        wait_for_other_path_to_settle(server_state, probe_id).await
    } else {
        server_state
            .probe_lifecycle_state(probe_id)
            .unwrap_or(ProbeDeliveryState::Queued)
    };
    tracing::info!(
        run_id,
        probe_id,
        state = state.as_str(),
        in_flight = in_flight_id.as_deref(),
        "interrupting probe delivery could not claim the probe; another dispatch path holds it or \
         the run's delivery slot",
    );
    InterruptingDelivery {
        state,
        interrupt: ProbeInterruptOutcome::NotAttempted,
        attempts: 0,
        detail: Some(
            "another delivery path claimed this probe (or the run's single in-flight slot) before \
             the interrupt could run; its state above is what that path recorded"
                .to_owned(),
        ),
    }
}

/// Poll `probe_id`'s lifecycle until it leaves `Queued`/`Injected`, or until
/// [`OTHER_PATH_SETTLE_TIMEOUT`]. The other path has already claimed this
/// probe; we are only waiting to report the outcome it records.
async fn wait_for_other_path_to_settle(server_state: &ServerState, probe_id: &str) -> ProbeDeliveryState {
    const POLL: Duration = Duration::from_millis(50);
    let deadline = tokio::time::Instant::now() + OTHER_PATH_SETTLE_TIMEOUT;
    loop {
        let state = server_state
            .probe_lifecycle_state(probe_id)
            .unwrap_or(ProbeDeliveryState::Queued);
        if !state.is_in_progress() {
            return state;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return state;
        }
        tokio::time::sleep(POLL.min(deadline.saturating_duration_since(now))).await;
    }
}

impl ServerState {
    /// Whether the worker pane for `run_id` currently shows **no** busy
    /// marker from its driver's [`boss_protocol::PaneMonitorSpec`].
    ///
    /// The second, independent read of "did the turn end" — the same literals
    /// the app's pane monitor screen-scrapes (`"esc to interrupt"` for Claude,
    /// `"Esc:cancel"` / `"[stop]"` for Grok).
    ///
    /// A `true` here is a **positive** reading, never merely an absence of
    /// evidence: the pane must show one of the driver's prompt prefixes *and*
    /// none of its busy markers. Answering `true` on an empty or unreadable
    /// capture is the same class of mistake as reporting a probe delivered
    /// because `SendToPane` returned `Ok` — it would have the engine typing
    /// into a turn that never stopped, which is exactly what confirming
    /// exists to prevent. So every uncertainty answers `false`:
    ///
    /// * a non-tmux (app-hosted) pane has no capture path here, so the
    ///   live-activity signal decides alone;
    /// * a driver that declares no pane-monitor spec, no busy markers, or no
    ///   prompt prefixes — there is nothing to read positively;
    /// * a capture that fails, or comes back blank.
    ///
    /// A `true` only ever *adds* a way to confirm the turn ended; it can never
    /// be the reason an interrupt is declared failed.
    pub(crate) async fn pane_text_shows_turn_ended(&self, run_id: &str) -> bool {
        let Some(pane) = self.worker_registry.pane_for_run(run_id) else {
            return false;
        };
        let Some(session_name) = pane.tmux_session_name else {
            return false;
        };
        let Some(driver) = crate::driver_transcript::driver_for_execution(&self.work_db, run_id) else {
            return false;
        };
        let Some(spec) = driver.pane_monitor_spec() else {
            return false;
        };
        if spec.busy_markers.is_empty() || spec.prompt_prefixes.is_empty() {
            return false;
        }
        let Ok(tmux) = self.tmux_for_pane_delivery(run_id) else {
            return false;
        };
        let Ok(text) = tmux.capture_pane(&session_name).await else {
            return false;
        };
        if text.trim().is_empty() {
            return false;
        }
        let lowered = text.to_lowercase();
        if spec
            .busy_markers
            .iter()
            .any(|marker| lowered.contains(&marker.to_lowercase()))
        {
            return false;
        }
        text.lines().any(|line| {
            spec.prompt_prefixes
                .iter()
                .any(|prefix| line.trim_start().starts_with(prefix))
        })
    }
}
