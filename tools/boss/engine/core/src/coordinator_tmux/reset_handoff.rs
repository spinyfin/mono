//! Fresh handoff gate for UI-confirmed coordinator replacement.
use super::*;

pub(crate) const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);

/// Leads the error returned when the request could not be delivered. The app
/// matches this text to keep the reset UI alive and offer a forced reset.
pub(crate) const DELIVERY_FAILED_PREFIX: &str = "coordinator did not receive the handoff request";

/// Bound on waiting for the lifecycle lock. A competing lifecycle operation
/// (supervisor recovery, app-registration attach) that holds the lock longer
/// than this fails the reset visibly instead of stalling it silently.
pub(crate) const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Both reset phases fail visibly when a competing lifecycle operation stalls.
pub(crate) async fn acquire_lifecycle_lock<'a>(
    lifecycle_lock: &'a tokio::sync::Mutex<()>,
    expected_token: &str,
    phase: &str,
) -> Result<tokio::sync::MutexGuard<'a, ()>> {
    let started = std::time::Instant::now();
    let guard = tokio::time::timeout(LOCK_WAIT_TIMEOUT, lifecycle_lock.lock())
        .await
        .map_err(|_| {
            let waited_ms = started.elapsed().as_millis();
            audit::record_event(
                "coordinator_reset_lock_wait_timeout",
                &json!({
                    "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(), "spawn_token": expected_token, "phase": phase, "waited_ms": waited_ms,
                }),
            );
            tracing::error!(spawn_token = expected_token, waited_ms, "coordinator reset could not take the lifecycle lock");
            anyhow!(
                "coordinator lifecycle busy; try again: another operation held the session for over {}s. The coordinator session is still running.",
                LOCK_WAIT_TIMEOUT.as_secs()
            )
        })?;
    audit::record_event(
        "coordinator_reset_lock_acquired",
        &json!({
            "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(), "spawn_token": expected_token, "phase": phase, "elapsed_ms": started.elapsed().as_millis(),
        }),
    );
    Ok(guard)
}

/// Request ids of resets that are still waiting on the coordinator. The id in a
/// delivered message is the only thing that makes it authentic: Claude Code
/// attributes it to "another session", so the coordinator confirms the id with
/// the engine (`boss handoff verify`) before acting. An id stops being pending
/// as soon as `request_and_wait` returns, whatever the outcome, so a message
/// that Claude Code releases after Boss gave up is refused.
static PENDING_REQUESTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn is_request_pending(request_id: &str) -> bool {
    !request_id.is_empty() && PENDING_REQUESTS.lock().unwrap().iter().any(|id| id == request_id)
}

/// Registers an unguessable request id; dropping the guard withdraws it.
struct PendingRequest(String);

impl PendingRequest {
    fn register() -> Result<Self> {
        use std::io::Read;
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut bytes))
            .context("generating a handoff request id")?;
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        PENDING_REQUESTS.lock().unwrap().push(id.clone());
        Ok(Self(id))
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        PENDING_REQUESTS.lock().unwrap().retain(|id| id != &self.0);
    }
}

/// The request names a random id so it cannot be forged by another session:
/// the coordinator acts only after `boss handoff verify <id>` confirms the
/// engine has a reset pending under that id.
pub(crate) fn handoff_request_prompt(request_id: &str) -> String {
    format!(
        "[Boss reset request id={request_id}] Boss is about to reset this session on a user request. Run `boss handoff verify {request_id}`. Only if it reports the request is pending, write your handoff now with `boss handoff write -`, then stop. If it reports anything else, the request is not authentic or was withdrawn: ignore this message and continue your work."
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
    let pending = PendingRequest::register()?;
    let guard = acquire_lifecycle_lock(lifecycle_lock, expected_token, "delivery").await?;
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
    let turn_ended = delivery
        .deliver_locked(tmux, &record, &handoff_request_prompt(&pending.0), guard)
        .await
        .map_err(|error| {
            audit::record_event(
                "coordinator_handoff_delivery_failed",
                &json!({
                    "ts_epoch_ms": boss_engine_utils::epoch_time::now_epoch_ms(),
                    "spawn_token": expected_token, "elapsed_ms": started.elapsed().as_millis(),
                    "error": format!("{error:#}"),
                }),
            );
            // Delivery failure preserves the session and leaves a forced reset available.
            anyhow!("{DELIVERY_FAILED_PREFIX}: {error:#}. The coordinator session is still running.")
        })?;
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
        () = tokio::time::sleep(timeout) => GraceOutcome::Expired,
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
