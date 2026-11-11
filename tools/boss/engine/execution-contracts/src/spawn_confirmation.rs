//! Typed spawn-confirmation failures.
//!
//! The runner raises these when a spawned worker never proves it started, and
//! the dispatch coordinator classifies them to word the `pane_spawn_failed`
//! attention. The type lives below both so the coordinator can match on it
//! without depending on the runner implementation.

use std::time::Duration;

/// Typed spawn-confirmation failure so the coordinator can classify the
/// attention body without matching error-message substrings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpawnConfirmationError {
    #[error(
        "refusing to complete {driver_name} spawn for {run_id}: driver composer never became ready within {timeout_secs}s \
         (no driver signal or pane marker after prompt delivery); recording a failed spawn"
    )]
    ComposerNotReady {
        driver_name: String,
        run_id: String,
        timeout_secs: u64,
    },
    #[error(
        "refusing to complete {driver_name} spawn for {run_id}: no driver hook or session event arrived within \
         {timeout_secs}s after prompt delivery; recording a failed spawn"
    )]
    TurnDidNotStart {
        driver_name: String,
        run_id: String,
        timeout_secs: u64,
    },
    /// The driver started and the brief reached its composer, but no turn
    /// began, even after Return was re-sent.
    #[error(
        "refusing to complete {driver_name} spawn for {run_id}: the initial prompt was delivered but the driver \
         never started a turn (no UserPromptSubmit or other turn event in {waited_secs}s, with Return re-sent \
         {retries} time(s)); recording a failed spawn"
    )]
    PromptNotSubmitted {
        driver_name: String,
        run_id: String,
        retries: u32,
        waited_secs: u64,
    },
    /// A remote driver started but reported no turn. The engine neither sees
    /// the remote launcher's delivery state nor can press Return there, so
    /// it knows neither whether the brief was pasted nor whether a retry
    /// would have helped.
    #[error(
        "refusing to complete {driver_name} spawn for {run_id}: the remote driver started but reported no turn \
         within {waited_secs}s; whether the initial prompt was delivered is unknown (the engine cannot see the \
         remote launcher) and Return could not be re-sent; recording a failed spawn"
    )]
    RemotePromptNotSubmitted {
        driver_name: String,
        run_id: String,
        waited_secs: u64,
    },
    /// The driver started but the launcher never got the brief to it.
    #[error(
        "refusing to complete {driver_name} spawn for {run_id}: the initial prompt was not delivered to the \
         driver ({reason}); recording a failed spawn"
    )]
    PromptNotDelivered {
        driver_name: String,
        run_id: String,
        reason: String,
    },
}

impl SpawnConfirmationError {
    /// True when the driver had started and what failed was getting it to
    /// take its prompt. Only a turn, not a driver signal, can then veto the
    /// reap.
    pub fn is_unsubmitted_prompt(&self) -> bool {
        matches!(
            self,
            Self::PromptNotSubmitted { .. } | Self::RemotePromptNotSubmitted { .. } | Self::PromptNotDelivered { .. }
        )
    }
}

/// Walk an anyhow chain for a [`SpawnConfirmationError`].
pub fn spawn_confirmation_error(err: &anyhow::Error) -> Option<&SpawnConfirmationError> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<SpawnConfirmationError>())
}

pub fn composer_not_ready_error(driver_name: &str, run_id: &str, timeout: Duration) -> anyhow::Error {
    anyhow::Error::from(SpawnConfirmationError::ComposerNotReady {
        driver_name: driver_name.to_owned(),
        run_id: run_id.to_owned(),
        timeout_secs: timeout.as_secs(),
    })
}

pub fn turn_did_not_start_error(driver_name: &str, run_id: &str, timeout: Duration) -> anyhow::Error {
    anyhow::Error::from(SpawnConfirmationError::TurnDidNotStart {
        driver_name: driver_name.to_owned(),
        run_id: run_id.to_owned(),
        timeout_secs: timeout.as_secs(),
    })
}

pub fn prompt_not_submitted_error(driver_name: &str, run_id: &str, retries: u32, waited: Duration) -> anyhow::Error {
    anyhow::Error::from(SpawnConfirmationError::PromptNotSubmitted {
        driver_name: driver_name.to_owned(),
        run_id: run_id.to_owned(),
        retries,
        waited_secs: waited.as_secs(),
    })
}

pub fn remote_prompt_not_submitted_error(driver_name: &str, run_id: &str, waited: Duration) -> anyhow::Error {
    anyhow::Error::from(SpawnConfirmationError::RemotePromptNotSubmitted {
        driver_name: driver_name.to_owned(),
        run_id: run_id.to_owned(),
        waited_secs: waited.as_secs(),
    })
}

pub fn prompt_not_delivered_error(driver_name: &str, run_id: &str, reason: impl Into<String>) -> anyhow::Error {
    anyhow::Error::from(SpawnConfirmationError::PromptNotDelivered {
        driver_name: driver_name.to_owned(),
        run_id: run_id.to_owned(),
        reason: reason.into(),
    })
}

/// Poll `ready` until it returns true or `timeout` elapses.
pub async fn wait_until<F, Fut>(timeout: Duration, poll: Duration, mut ready: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::timeout_at(deadline, ready()).await.unwrap_or(false) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline - now;
        tokio::time::sleep(poll.min(remaining)).await;
    }
}

/// Remote prompt-accepted wait, then `reap` when no turn begins.
///
/// The engine cannot press Return in a remote worker's terminal, so there is
/// no retry here: the launcher's own Return, sent once the composer shows the
/// brief, is the only submit. A worker that still has not begun a turn after
/// `timeout` is reaped and its spawn failed rather than left idle.
pub async fn confirm_prompt_accepted_or_reap<Accepted, AcceptedFut, Reap, ReapFut>(
    driver_name: &str,
    run_id: &str,
    timeout: Duration,
    poll: Duration,
    accepted: Accepted,
    reap: Reap,
) -> anyhow::Result<()>
where
    Accepted: FnMut() -> AcceptedFut,
    AcceptedFut: std::future::Future<Output = bool>,
    Reap: FnOnce() -> ReapFut,
    ReapFut: std::future::Future<Output = ()>,
{
    let started = tokio::time::Instant::now();
    if wait_until(timeout, poll, accepted).await {
        tracing::info!(
            run_id,
            driver = driver_name,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "prompt submit confirmed: the driver started a turn",
        );
        return Ok(());
    }
    tracing::warn!(
        run_id,
        driver = driver_name,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "prompt submit failed: the remote driver started but reported no turn",
    );
    reap().await;
    Err(remote_prompt_not_submitted_error(driver_name, run_id, timeout))
}
