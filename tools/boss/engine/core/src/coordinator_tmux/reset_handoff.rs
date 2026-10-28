//! Fresh handoff gate for UI-confirmed coordinator replacement.
use super::*;

pub(crate) const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);

/// Leads the error returned when the request could not be delivered. The app
/// matches this text to keep the reset UI alive and offer a forced reset.
pub(crate) const DELIVERY_FAILED_PREFIX: &str = "coordinator did not receive the handoff request";

/// The `[Boss reset request]` marker lets the coordinator recognise this as
/// engine-originated despite Claude attributing it to another session.
///
/// The request carries an expiry (epoch seconds). A message that Claude Code
/// held past the delivery deadline can be released later, after Boss has
/// reported failure and no reset is pending; the coordinator ignores a request
/// whose expiry has passed.
pub(crate) fn handoff_request_prompt(now_epoch_secs: i64, authorization: &str) -> String {
    let expires = now_epoch_secs + (super::inbox::DELIVERY_TIMEOUT.as_secs() + 15) as i64;
    format!(
        "[Boss reset request expires={expires}] Boss is about to reset this session on a user request. Write your handoff now with `boss handoff write --authorization {authorization} -`, then stop. If `date +%s` is already past the expiry, the request was withdrawn: ignore it and continue your work."
    )
}

#[derive(bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct HandoffRequest<'a> {
    work_db: &'a WorkDb,
    tmux: &'a Tmux,
    lifecycle_lock: &'a tokio::sync::Mutex<()>,
    written: &'a tokio::sync::Notify,
    expected_token: &'a str,
    force: bool,
    timeout: Duration,
    delivery: &'a dyn super::inbox::HandoffDelivery,
}

/// Validates and sends the request under `lifecycle_lock`, so the supervisor
/// cannot replace the session between the token check and the prompt, then
/// releases it for the (long) wait. Recreation must recheck the token under
/// the lock after this returns. Errors leave the session untouched.
pub(crate) async fn request_and_wait(request: HandoffRequest<'_>) -> Result<bool> {
    let HandoffRequest {
        work_db,
        tmux,
        lifecycle_lock,
        written,
        expected_token,
        force,
        timeout,
        delivery,
    } = request;
    let started = std::time::Instant::now();
    audit::record_event(
        "coordinator_handoff_delivery_started",
        &json!({ "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(), "spawn_token": expected_token }),
    );
    let guard = lifecycle_lock.lock().await;
    audit::record_event(
        "coordinator_handoff_lock_acquired",
        &json!({
            "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(), "spawn_token": expected_token, "elapsed_ms": started.elapsed().as_millis(),
        }),
    );
    let record = work_db
        .coordinator_tmux_record()?
        .ok_or_else(|| anyhow!("no coordinator tmux record exists"))?;
    if record.spawn_token != expected_token {
        bail!("coordinator changed before confirmation; refresh and confirm the current session instead");
    }
    let skip = if force {
        Some("forced_without_handoff")
    } else if !session_exists(tmux, &record.session_name).await? {
        Some("session_absent")
    } else {
        if tmux
            .show_environment(&record.session_name, SPAWN_TOKEN_ENV)
            .await?
            .as_deref()
            != Some(expected_token)
        {
            bail!("coordinator tmux token does not match the metadata singleton");
        }
        if tmux
            .display_message(&record.session_name, DisplayField::PaneDead)
            .await?
            .trim()
            == "1"
        {
            Some("pane_dead")
        } else {
            None
        }
    };
    if let Some(reason) = skip {
        audit::record_event(
            "coordinator_handoff_skipped",
            &json!({
                "reason": reason, "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(), "spawn_token": expected_token,
            }),
        );
        return Ok(!force);
    }
    let requested_at = boss_engine_utils::epoch_time::now_epoch_secs();
    let previous_write = match work_db.coordinator_handoff_state() {
        HandoffState::Present(h) => h.write_id,
        _ => String::new(),
    };
    // Each step names itself in the error: the app shows that text verbatim
    // so the operator learns which step failed and why, not only that the
    // request never arrived.
    let authorization = work_db
        .ensure_coordinator_handoff_authorization(&record.spawn_token)
        .map_err(|error| {
            audit::record_event(
                "coordinator_handoff_delivery_failed",
                &json!({
                    "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(),
                    "spawn_token": expected_token, "elapsed_ms": started.elapsed().as_millis(),
                    "step": "credential", "error": format!("{error:#}"),
                }),
            );
            anyhow!(
                "{DELIVERY_FAILED_PREFIX}: the handoff credential for this session could not be minted ({error:#}). The coordinator session is still running."
            )
        })?;
    let turn_ended = delivery
        .deliver_locked(
            tmux,
            &record,
            &handoff_request_prompt(requested_at, &authorization),
            guard,
        )
        .await
        .map_err(|error| {
            audit::record_event(
                "coordinator_handoff_delivery_failed",
                &json!({
                    "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(),
                    "spawn_token": expected_token, "elapsed_ms": started.elapsed().as_millis(),
                    "step": "inbox_delivery", "error": format!("{error:#}"),
                }),
            );
            // Delivery failure preserves the session and leaves a forced reset available.
            anyhow!(
                "{DELIVERY_FAILED_PREFIX}: inbox delivery failed: {error:#}. The coordinator session is still running."
            )
        })?;
    // The request carried the credential, so the live session now holds it.
    if let Err(error) = work_db.record_coordinator_handoff_authorization_delivered(&record.spawn_token) {
        tracing::warn!(error = %format!("{error:#}"), "could not record handoff credential delivery after a reset request");
    }
    audit::record_event(
        "coordinator_handoff_requested",
        &json!({
            "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(), "spawn_token": expected_token, "requested_at": requested_at,
            "delivery_elapsed_ms": started.elapsed().as_millis(),
        }),
    );
    audit::record_event(
        "coordinator_handoff_grace_started",
        &json!({
            "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(), "spawn_token": expected_token, "timeout_ms": timeout.as_millis(),
        }),
    );
    let wait_for_write = async {
        loop {
            // Register before checking storage so a write between the check
            // and await cannot be lost, including concurrent reset waiters.
            let notified = written.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if fresh_write(work_db, expected_token, requested_at, &previous_write) {
                break;
            }
            notified.await;
        }
    };
    // The turn is the one the delivered request started, so idleness from
    // before delivery cannot end the wait early.
    let outcome = tokio::select! {
        () = wait_for_write => GraceOutcome::Written,
        () = turn_ended => {
            if fresh_write(work_db, expected_token, requested_at, &previous_write) {
                GraceOutcome::Written
            } else {
                GraceOutcome::TurnEndedWithoutWrite
            }
        }
        () = tokio::time::sleep(timeout) => {
            // A persisted write and the deadline can both be ready when this
            // task is polled. Selection order must not label that write stale.
            if fresh_write(work_db, expected_token, requested_at, &previous_write) {
                GraceOutcome::Written
            } else {
                GraceOutcome::Expired
            }
        }
    };
    if outcome != GraceOutcome::Written {
        let (event, why) = match outcome {
            GraceOutcome::TurnEndedWithoutWrite => (
                "coordinator_handoff_turn_ended_without_write",
                "the coordinator's turn ended without writing a handoff",
            ),
            _ => ("coordinator_handoff_timeout", "grace period expired"),
        };
        audit::record_event(
            event,
            &json!({
                "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(), "spawn_token": expected_token, "requested_at": requested_at,
            }),
        );
        tracing::error!(
            spawn_token = expected_token,
            timeout_secs = timeout.as_secs(),
            "coordinator reset proceeding WITHOUT a fresh handoff: {why}"
        );
        return Ok(false);
    }
    audit::record_event(
        "coordinator_handoff_received",
        &json!({
            "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(), "spawn_token": expected_token, "requested_at": requested_at,
        }),
    );
    Ok(true)
}

#[derive(PartialEq, Eq)]
enum GraceOutcome {
    Written,
    TurnEndedWithoutWrite,
    Expired,
}

fn fresh_write(work_db: &WorkDb, expected_token: &str, requested_at: i64, previous_write: &str) -> bool {
    matches!(work_db.coordinator_handoff_state(), HandoffState::Present(h)
        if h.writer_spawn_token == expected_token && h.written_at >= requested_at && h.write_id != previous_write)
}
