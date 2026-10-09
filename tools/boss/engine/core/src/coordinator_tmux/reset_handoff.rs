//! Fresh handoff gate for operator-confirmed coordinator replacement.
use super::*;

pub(crate) const HANDOFF_TIMEOUT: Duration = Duration::from_secs(120);

/// Called outside the lifecycle lock; recreation must recheck the token under
/// that lock after this returns. Errors leave the session untouched.
pub(crate) async fn request_and_wait(
    work_db: &WorkDb,
    tmux: &Tmux,
    written: &tokio::sync::Notify,
    expected_token: &str,
    force: bool,
    timeout: Duration,
) -> Result<()> {
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
                "reason": reason, "spawn_token": expected_token,
            }),
        );
        return Ok(());
    }
    let requested_at = boss_engine_utils::epoch_time::now_epoch_secs();
    tmux.send_keys(&record.session_name,
        "Boss is about to reset this session at the operator's request. Write your handoff now with `boss handoff write -`, then stop."
    ).await?;
    audit::record_event(
        "coordinator_handoff_requested",
        &json!({
            "spawn_token": expected_token, "requested_at": requested_at,
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
                if h.writer_spawn_token == expected_token && h.written_at >= requested_at)
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
                "spawn_token": expected_token, "requested_at": requested_at,
            }),
        );
        bail!("coordinator did not write a handoff within {}s", timeout.as_secs());
    }
    audit::record_event(
        "coordinator_handoff_received",
        &json!({
            "spawn_token": expected_token, "requested_at": requested_at,
        }),
    );
    Ok(())
}
