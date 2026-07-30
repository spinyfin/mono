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

use anyhow::{Context, Result, bail};
use boss_protocol::{FrontendEvent, FrontendRequest};

use crate::{agents, connect};

/// Queue a probe for the worker named by `agent`, reporting honestly what the
/// engine committed to — or failing when it committed to nothing.
pub async fn probe_run(
    socket_path: &Option<String>,
    json: bool,
    agent: String,
    text: String,
    urgent: bool,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = agents::fetch_live_states(&mut client).await?;
    let run_id = agents::resolve_agent_ref(&agent, &states)?.run_id.clone();
    let response = client
        .send_request(&FrontendRequest::ProbeRun {
            run_id: run_id.clone(),
            text,
            urgent,
        })
        .await
        .context("sending ProbeRun")?;
    match response {
        FrontendEvent::ProbeQueued {
            run_id: returned,
            probe_id,
            urgent: is_urgent,
            expected_delivery,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "accepted",
                        "run_id": returned,
                        "probe_id": probe_id,
                        "urgent": is_urgent,
                        "expected_delivery": expected_delivery.map(|e| e.as_str()),
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
/// An *undeliverable* state (`dropped` / `abandoned` / `orphaned`) also gets a
/// stderr line, and that one IS a redelivery cue: the engine gave up before
/// the text reached a live worker, so nothing landed and re-issuing cannot
/// duplicate an instruction. These states exist so that a probe the engine
/// stopped trying to deliver stops reporting `queued` — `queued` is a live
/// promise, and a probe stuck on it against a finished run was the bug.
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
            if state == boss_protocol::ProbeDeliveryState::Unconfirmed {
                eprintln!(
                    "warning: probe {returned} is unconfirmed: the write reached the pane but the engine could \
                     not prove the worker took it. It may still have landed — check the worker's transcript \
                     before re-issuing, since a second copy would repeat the instruction."
                );
            } else if state.is_undeliverable() {
                eprintln!(
                    "warning: probe {returned} was never delivered (state={}): the engine gave up on it before \
                     the text reached a live worker. Nothing landed, so re-issuing against a live run is safe.",
                    state.as_str(),
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
