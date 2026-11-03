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
