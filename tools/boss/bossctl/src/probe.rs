//! `bossctl probe` / `bossctl probe-status` — inject text into a live
//! worker's pane, and read what became of it.
//!
//! These two commands are deliberately written so that a probe which will not
//! be delivered *fails*. The engine decides deliverability before it accepts
//! anything and answers `ProbeRefused` when it cannot commit to a boundary;
//! [`probe_run`] turns that into a non-zero exit. It must never print a
//! warning and exit 0: the whole point is that "queued, arriving shortly" and
//! "never going to arrive" have to look different from the outside, since
//! acting on the difference means either waiting or restarting the worker.
//!
//! There is a third answer between those two, and it is the one that bit:
//! *accepted, but the engine cannot promise the delivery boundary will ever
//! arrive.* The engine reports it as
//! [`boss_protocol::ProbeDeliveryExpectation::is_best_effort`], and
//! [`probe_run`] prints the caveat on stderr at issue time rather than
//! leaving the caller to discover it from a `probe-status` query it has no
//! reason to run. Exit stays 0 — the probe genuinely may land, and refusing
//! outright would remove the surface without fixing the capability.
//!
//! **Interrupting is the default**, and it removes that third answer
//! entirely for the case it hurt most. With `interrupt` the engine cuts the
//! worker's in-flight turn short, confirms the turn ended, writes the text
//! and confirms the write — all inside the RPC — so this command reports the
//! *settled* [`boss_protocol::ProbeDeliveryState`] instead of an intention.
//! A genuine delivery failure (nothing reached the pane) exits non-zero.
//! `unconfirmed` (written, unproven) and `queued`/`injected` (still in
//! flight) are **not** reported as "NOT delivered": that wording is what
//! drove the coordinator to re-send a probe that had already landed.
//! `--no-interrupt` opts back into boundary delivery for a message that
//! genuinely can wait, since an interrupt aborts whatever the worker was
//! doing.

use anyhow::{Context, Result, bail};
use boss_protocol::{FrontendEvent, FrontendRequest, ProbeDeliveryState};

use crate::{agents, connect};

/// How the interrupting `ProbeRun` answer should be described.
///
/// Three answers, not two. Collapsing "written but unproven" and "still
/// queued" into "NOT delivered" is what sent the coordinator chasing
/// probes that had already landed — re-sending them duplicated the
/// instruction, and skipping them risked losing one the engine had not
/// actually failed to deliver.
enum InterruptingStatus {
    Delivered,
    Unconfirmed,
    InProgress,
    NotDelivered,
}

impl InterruptingStatus {
    fn as_json_status(&self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Unconfirmed => "unconfirmed",
            Self::InProgress => "in_progress",
            Self::NotDelivered => "not_delivered",
        }
    }

    fn as_verdict(&self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Unconfirmed => "written, unconfirmed",
            Self::InProgress => "not yet settled",
            Self::NotDelivered => "NOT delivered",
        }
    }
}

fn interrupting_status(state: ProbeDeliveryState) -> InterruptingStatus {
    if state.is_delivered() {
        InterruptingStatus::Delivered
    } else if state == ProbeDeliveryState::Unconfirmed {
        // Defensive: the interrupting path normally settles a successful
        // parked write as Consumed after its liveness check.
        InterruptingStatus::Unconfirmed
    } else if state.is_in_progress() {
        InterruptingStatus::InProgress
    } else {
        InterruptingStatus::NotDelivered
    }
}

/// Send a probe to the worker named by `agent`.
///
/// With `interrupt` (the default from the CLI) the engine cuts the worker's
/// in-flight turn short and delivers synchronously, so this reports the
/// settled delivery state and exits non-zero when the text did not land.
/// Without it the engine queues the probe for a boundary and this reports
/// honestly what the engine *committed to* — or fails when it committed to
/// nothing.
pub async fn probe_run(
    socket_path: &Option<String>,
    json: bool,
    agent: String,
    text: String,
    urgent: bool,
    interrupt: bool,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = agents::fetch_live_states(&mut client).await?;
    let run_id = agents::resolve_agent_ref(&agent, &states)?.run_id.clone();
    let response = client
        .send_request(&FrontendRequest::ProbeRun {
            run_id: run_id.clone(),
            text,
            urgent,
            interrupt,
        })
        .await
        .context("sending ProbeRun")?;
    match response {
        // Interrupting delivery: the engine already finished, so this is the
        // real outcome, not an acceptance. Report it as such — and fail when
        // it failed, since the whole reason to interrupt is that the message
        // had to land, and a caller that cannot tell "landed" from "did not"
        // is back where the boundary-only design left them.
        FrontendEvent::ProbeDelivered {
            run_id: returned,
            probe_id,
            urgent: is_urgent,
            state,
            interrupt: interrupt_outcome,
            interrupt_attempts,
            detail,
        } => {
            let status = interrupting_status(state);
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": status.as_json_status(),
                        "run_id": returned,
                        "probe_id": probe_id,
                        "urgent": is_urgent,
                        "state": state.as_str(),
                        "delivered": state.is_delivered(),
                        "interrupt": interrupt_outcome.as_str(),
                        "interrupt_attempts": interrupt_attempts,
                        "interrupted_worker": interrupt_outcome.discarded_in_flight_work(),
                        "safe_to_reissue": state.is_safe_to_reissue(),
                        "detail": detail,
                    })
                );
            } else {
                let label = if is_urgent { "urgent probe" } else { "probe" };
                println!(
                    "{label} {} to run {returned} (probe_id={probe_id}); state={}",
                    status.as_verdict(),
                    state.as_str(),
                );
                println!("  interrupt: {}", interrupt_outcome.describe());
                if interrupt_attempts > 0 {
                    println!("  interrupt attempts: {interrupt_attempts}");
                }
                if let Some(detail) = detail.as_deref() {
                    println!("  {detail}");
                }
            }
            if state.is_delivered() {
                return Ok(());
            }
            if state == ProbeDeliveryState::Unconfirmed {
                // Written, unproven. Exit 0 so a coordinator does not treat
                // this as a loss and re-send — a second copy repeats the
                // instruction. Distinct from "NOT delivered" on purpose.
                eprintln!(
                    "warning: probe {probe_id} was written into the pane but delivery was not \
                     confirmed (state=unconfirmed). The text may have reached the worker — check \
                     the transcript before re-issuing, since a second copy would repeat the \
                     instruction."
                );
                return Ok(());
            }
            if state.is_in_progress() {
                eprintln!(
                    "warning: probe {probe_id} is not yet settled (state={}); another delivery \
                     path holds it or the write is still in flight. This is not a confirmed loss \
                     — do not re-issue until probe-status shows a terminal state.",
                    state.as_str(),
                );
                return Ok(());
            }
            // Actually undeliverable. Say what to do instead rather than only
            // what went wrong: for the states where nothing reached the pane,
            // re-issuing cannot duplicate an instruction, and for a probe
            // that must be obeyed the durable channel is a work-item
            // description edit, not this one.
            let advice = if state.is_safe_to_reissue() {
                "Nothing was written into the worker's pane, so re-issuing this probe cannot \
                 duplicate an instruction. If it must be obeyed, prefer a channel that survives \
                 the run (edit the work item's description)."
            } else {
                "The text may have reached the pane — check the worker's transcript before \
                 re-issuing, since a second copy would repeat the instruction."
            };
            bail!(
                "probe {probe_id} was not delivered to run {returned} (state={}): {}{}. {advice}",
                state.as_str(),
                interrupt_outcome.describe(),
                detail.map(|d| format!("; {d}")).unwrap_or_default(),
            )
        }
        FrontendEvent::ProbeQueued {
            run_id: returned,
            probe_id,
            urgent: is_urgent,
            expected_delivery,
        } => {
            // Acceptance is not delivery, and for some drivers it is not even
            // a promise of delivery — see `ProbeDeliveryExpectation::
            // is_best_effort`. An engine too old to report an expectation is
            // treated as best-effort: "we don't know" must warn, not reassure.
            let best_effort = expected_delivery.is_none_or(|e| e.is_best_effort());
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "accepted",
                        "run_id": returned,
                        "probe_id": probe_id,
                        "urgent": is_urgent,
                        "expected_delivery": expected_delivery.map(|e| e.as_str()),
                        "best_effort": best_effort,
                    })
                );
            } else {
                // Print the boundary the engine committed to rather than
                // inferring one from the urgent flag.
                let when = expected_delivery
                    .map(|e| e.describe().to_owned())
                    .unwrap_or_else(|| "delivery boundary not reported by this engine".to_owned());
                let label = if is_urgent { "urgent probe" } else { "probe" };
                println!("{label} accepted for run {returned} (probe_id={probe_id}); {when}");
                println!("check delivery with: bossctl probe-status {probe_id}");
            }
            // On stderr in both modes, so `--json` stdout stays parseable and
            // a caller who only reads the exit code and stdout still sees it
            // in a terminal. Exit stays 0: the engine did accept, and the
            // probe may well land — the caller is being told that "accepted"
            // is weaker here than it looks, not that the call failed.
            if best_effort {
                let caveat = expected_delivery
                    .and_then(boss_protocol::ProbeDeliveryExpectation::caveat)
                    .unwrap_or(
                        "this engine did not report a delivery expectation, so the engine cannot say \
                     whether the text will reach the worker.",
                    );
                eprintln!("warning: delivery of probe {probe_id} is best-effort and may never occur: {caveat}");
            }
            Ok(())
        }
        // Refusal is a failure, not a warning: exit non-zero so a script (or
        // a coordinator) cannot mistake it for an accepted probe.
        FrontendEvent::ProbeRefused {
            run_id: returned,
            reason,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "refused",
                        "run_id": returned,
                        "urgent": urgent,
                        "reason": reason,
                    })
                );
            }
            bail!("engine refused probe for run {returned}: {reason}")
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected probe: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Read the delivery state the engine recorded for `probe_id`.
///
/// The exit code answers "did the read work?", not "did the probe land?": any
/// state the engine could report exits 0, and only an unreadable or unknown
/// probe id exits non-zero. The delivery judgement travels in the `delivered`
/// field (`--json`) or the printed `state=`, so a consumer gets one predicate
/// per question instead of an exit code that means both.
///
/// `dropped` and `abandoned` also get a stderr line, and that one IS a
/// redelivery cue: the engine gave up before the text ever reached a live
/// worker (queued but never written, or the run went away first), so nothing
/// landed and re-issuing cannot duplicate an instruction. `orphaned` is
/// undeliverable too but is a different case: the write reached the pane, and
/// either a fragile `kill(pid, 0)` liveness check says nobody was left to read
/// it, or the run's pane was released before the worker produced the boundary
/// its reply would have arrived on. Neither is conclusive — a reply that does
/// arrive still corrects the record to `Replied` — so `orphaned` gets its own,
/// more cautious stderr line instead of the safe-to-reissue one. These states
/// exist so that a probe the engine stopped trying to deliver stops reporting
/// `queued` — `queued` is a live promise, and a probe stuck on it against a
/// finished run was the bug.
///
/// `unconfirmed` still gets an explanatory line on stderr: the engine wrote
/// the text but could not prove the worker took it. That is deliberately
/// *not* a redelivery instruction — the engine does not auto-redeliver an
/// unconfirmed probe because the text may well have landed, and a second copy
/// would hand the worker the same instruction twice. Re-issuing is a
/// judgement call to be made with the worker's transcript in hand.
///
/// The loud-failure requirement for undeliverable probes is met up front by
/// [`probe_run`], which refuses before a probe id is ever minted.
pub async fn probe_status(socket_path: &Option<String>, json: bool, probe_id: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::ProbeStatus {
            probe_id: probe_id.clone(),
        })
        .await
        .context("sending ProbeStatus")?;
    match response {
        FrontendEvent::ProbeStatusResult {
            run_id,
            probe_id: returned,
            state,
            urgent,
            detail,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "probe_id": returned,
                        "run_id": run_id,
                        "state": state.as_str(),
                        "urgent": urgent,
                        "delivered": state.is_delivered(),
                        "detail": detail,
                    })
                );
            } else {
                let urgency = if urgent { " urgent" } else { "" };
                println!("{returned}:{urgency} run={run_id} state={}", state.as_str());
                if let Some(detail) = detail.as_deref() {
                    println!("  {detail}");
                }
            }
            // The read succeeded, so this exits 0 whatever the state is —
            // `delivered` / `state=` carry the judgement. Unconfirmed still
            // warrants a warning, on stderr so it does not corrupt `--json`
            // stdout.
            if state == boss_protocol::ProbeDeliveryState::InterruptFailed {
                eprintln!(
                    "warning: probe {returned} was never delivered (state=interrupt_failed): the engine \
                     interrupted the worker's turn to deliver it, the turn never ended within the driver's \
                     declared attempt budget, and nothing was written into the pane. Re-issuing is safe — \
                     nothing landed — but it will hit the same wall unless the worker's state has changed; \
                     prefer a channel that survives the run (edit the work item's description).",
                );
            } else if state == boss_protocol::ProbeDeliveryState::Unconfirmed {
                eprintln!(
                    "warning: probe {returned} is unconfirmed: the write reached the pane but the engine could \
                     not prove the worker took it. It may still have landed — check the worker's transcript \
                     before re-issuing, since a second copy would repeat the instruction."
                );
            } else if matches!(
                state,
                boss_protocol::ProbeDeliveryState::Dropped | boss_protocol::ProbeDeliveryState::Abandoned
            ) {
                eprintln!(
                    "warning: probe {returned} was never delivered (state={}): the engine gave up on it before \
                     the text reached a live worker. Nothing landed, so re-issuing against a live run is safe.",
                    state.as_str(),
                );
            } else if state == boss_protocol::ProbeDeliveryState::Orphaned {
                eprintln!(
                    "warning: probe {returned} is orphaned (state=orphaned): the write reached the pane but \
                     nothing was left to act on it — the worker's recorded process had already exited, or its \
                     run was torn down before it answered. It most likely went unread, but neither verdict is \
                     conclusive — check the worker's transcript before re-issuing, since a reply that does \
                     arrive still corrects the record to `replied`.",
                );
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine could not report probe status: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupting_status_does_not_call_unconfirmed_or_queued_not_delivered() {
        assert_eq!(
            interrupting_status(ProbeDeliveryState::Consumed).as_json_status(),
            "delivered"
        );
        assert_eq!(
            interrupting_status(ProbeDeliveryState::Buffered).as_json_status(),
            "delivered"
        );
        assert_eq!(
            interrupting_status(ProbeDeliveryState::Replied).as_json_status(),
            "delivered"
        );
        assert_eq!(
            interrupting_status(ProbeDeliveryState::Unconfirmed).as_json_status(),
            "unconfirmed"
        );
        assert_eq!(
            interrupting_status(ProbeDeliveryState::Queued).as_json_status(),
            "in_progress"
        );
        assert_eq!(
            interrupting_status(ProbeDeliveryState::Injected).as_json_status(),
            "in_progress"
        );
        assert_eq!(
            interrupting_status(ProbeDeliveryState::InterruptFailed).as_json_status(),
            "not_delivered"
        );
        assert_eq!(
            interrupting_status(ProbeDeliveryState::Abandoned).as_json_status(),
            "not_delivered"
        );
        assert_eq!(
            interrupting_status(ProbeDeliveryState::Dropped).as_json_status(),
            "not_delivered"
        );
        assert_eq!(
            interrupting_status(ProbeDeliveryState::Orphaned).as_json_status(),
            "not_delivered"
        );
    }

    #[test]
    fn interrupting_verdict_never_says_not_delivered_for_a_write_that_landed() {
        assert_eq!(
            interrupting_status(ProbeDeliveryState::Unconfirmed).as_verdict(),
            "written, unconfirmed"
        );
        assert_eq!(
            interrupting_status(ProbeDeliveryState::Queued).as_verdict(),
            "not yet settled"
        );
        assert_eq!(
            interrupting_status(ProbeDeliveryState::Consumed).as_verdict(),
            "delivered"
        );
        assert_eq!(
            interrupting_status(ProbeDeliveryState::InterruptFailed).as_verdict(),
            "NOT delivered"
        );
        assert!(
            !interrupting_status(ProbeDeliveryState::Unconfirmed)
                .as_verdict()
                .contains("NOT delivered"),
            "unconfirmed must not read as a loss"
        );
        assert!(
            !interrupting_status(ProbeDeliveryState::Queued)
                .as_verdict()
                .contains("NOT delivered"),
            "queued must not read as a loss"
        );
    }
}
