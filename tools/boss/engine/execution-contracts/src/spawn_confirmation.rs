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
        matches!(self, Self::PromptNotSubmitted { .. } | Self::PromptNotDelivered { .. })
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

pub fn prompt_not_delivered_error(driver_name: &str, run_id: &str, reason: impl Into<String>) -> anyhow::Error {
    anyhow::Error::from(SpawnConfirmationError::PromptNotDelivered {
        driver_name: driver_name.to_owned(),
        run_id: run_id.to_owned(),
        reason: reason.into(),
    })
}
