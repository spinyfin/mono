//! Fresh handoff gate for UI-confirmed coordinator replacement.
use super::*;

pub(crate) const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);

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
    delivery.deliver(tmux, &record,
        "Boss is about to reset this session at the operator's request. Write your handoff now with `boss handoff write -`, then stop."
    ).await.map_err(|error| {
        audit::record_event("coordinator_handoff_delivery_failed", &json!({
            "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(),
            "spawn_token": expected_token, "elapsed_ms": started.elapsed().as_millis(),
            "error": format!("{error:#}"),
        }));
        error
    })?;
    audit::record_event(
        "coordinator_handoff_requested",
        &json!({
            "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(), "spawn_token": expected_token, "requested_at": requested_at,
            "delivery_elapsed_ms": started.elapsed().as_millis(),
        }),
    );
    drop(guard);
    audit::record_event(
        "coordinator_handoff_grace_started",
        &json!({
            "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(), "spawn_token": expected_token, "timeout_ms": timeout.as_millis(),
        }),
    );
    let result = tokio::time::timeout(timeout, async {
        loop {
            // Register before checking storage so a write between the check
            // and await cannot be lost, including concurrent reset waiters.
            let notified = written.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if matches!(work_db.coordinator_handoff_state(), HandoffState::Present(h)
                if h.writer_spawn_token == expected_token && h.written_at >= requested_at && h.write_id != previous_write)
            {
                break;
            }
            notified.await;
        }
    })
    .await;
    if result.is_err() {
        audit::record_event(
            "coordinator_handoff_timeout",
            &json!({
                "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(), "spawn_token": expected_token, "requested_at": requested_at,
            }),
        );
        tracing::error!(
            spawn_token = expected_token,
            timeout_secs = timeout.as_secs(),
            "coordinator reset proceeding WITHOUT a fresh handoff: grace period expired"
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
