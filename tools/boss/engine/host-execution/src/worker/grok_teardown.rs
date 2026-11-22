//! Bounded Grok ACP cancel-and-wait before a host-owned `kill_session_verified`.
//!
//! Spoke Grok workers are stopped here, not by the engine's local tmux reap.
//! An open turn must be cancelled with the driver's interrupt plan and given a
//! short window to emit `turn_completed` into `updates.jsonl` so the spoke
//! transcript observer can journal usage before the session is destroyed.

use super::exact_target;
use boss_engine_driver::DriverRegistry;
use boss_engine_driver::grok::{
    GROK_TEARDOWN_POLL_INTERVAL, GROK_TEARDOWN_SETTLE_WINDOW, GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON, GrokSettleStatus,
    GrokSettleTails, GrokSettleTransport, GrokTurnProgress, settle_open_turn, timeout_remaining,
};
use boss_tmux::Tmux;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrokSettleOutcome {
    Skipped,
    Completed,
    TimedOut,
}

impl GrokSettleOutcome {
    pub(crate) fn usage_unavailable_reason(self) -> Option<String> {
        match self {
            Self::TimedOut => Some(GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON.to_owned()),
            Self::Skipped | Self::Completed => None,
        }
    }
}

fn settle_window() -> Duration {
    #[cfg(test)]
    if let Some(window) = SETTLE_WINDOW_OVERRIDE.with(std::cell::Cell::get) {
        return window;
    }
    GROK_TEARDOWN_SETTLE_WINDOW
}

#[cfg(test)]
thread_local! {
    static SETTLE_WINDOW_OVERRIDE: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct SettleWindowGuard;

#[cfg(test)]
impl Drop for SettleWindowGuard {
    fn drop(&mut self) {
        SETTLE_WINDOW_OVERRIDE.with(|cell| cell.set(None));
    }
}

#[cfg(test)]
pub(crate) fn override_settle_window(window: Duration) -> SettleWindowGuard {
    SETTLE_WINDOW_OVERRIDE.with(|cell| cell.set(Some(window)));
    SettleWindowGuard
}

pub(crate) async fn settle_open_grok_turn(
    tmux: &Tmux,
    session: &str,
    grok_home: Option<&Path>,
    execution_id: &str,
    driver_id: &str,
) -> GrokSettleOutcome {
    if driver_id != boss_engine_driver::grok::TRANSCRIPT_DRIVER_SLUG {
        return GrokSettleOutcome::Skipped;
    }
    let Ok(driver) = DriverRegistry::default().require(driver_id) else {
        return GrokSettleOutcome::Skipped;
    };
    let Some(home) = grok_home.map(Path::to_path_buf) else {
        return GrokSettleOutcome::Skipped;
    };
    let window = settle_window();
    let deadline = tokio::time::Instant::now() + window;
    let session_files = match timeout_remaining(
        deadline,
        tokio::task::spawn_blocking(move || boss_engine_driver::grok::session_files_for_home(&home)),
    )
    .await
    {
        Some(Ok(Some(paths))) => paths,
        Some(Ok(None)) => return GrokSettleOutcome::Skipped,
        Some(Err(_)) | None => {
            tracing::warn!(
                execution_id,
                session,
                settle_window_secs = window.as_secs_f64(),
                reason = GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON,
                "grok teardown: resolving session files timed out; proceeding with kill",
            );
            return GrokSettleOutcome::TimedOut;
        }
    };

    struct HostSettleTransport<'a> {
        tails: GrokSettleTails,
        tmux: &'a Tmux,
        target: String,
        execution_id: &'a str,
        session: &'a str,
    }

    #[async_trait::async_trait]
    impl GrokSettleTransport for HostSettleTransport<'_> {
        type Error = anyhow::Error;

        async fn poll(&mut self, progress: &mut GrokTurnProgress) {
            self.tails.poll(progress).await;
        }

        async fn send_key(&mut self, key: &'static str) -> Result<(), Self::Error> {
            self.tmux.send_key(&self.target, key).await
        }

        fn on_cancel_start(&mut self, progress: &GrokTurnProgress) {
            tracing::info!(
                execution_id = self.execution_id,
                session = self.session,
                turn_started = progress.events.started,
                turn_ended = progress.events.ended,
                turn_completed = progress.updates.completed,
                "grok teardown: cancelling open Grok ACP turn before kill",
            );
        }

        fn on_send_error(&mut self, error: Self::Error) {
            tracing::warn!(
                execution_id = self.execution_id,
                session = self.session,
                error = %format!("{error:#}"),
                "grok teardown: failed sending interrupt to Grok session before kill; skipping settle wait",
            );
        }
    }

    let mut transport = HostSettleTransport {
        tails: GrokSettleTails::new(Some(session_files.0), Some(session_files.1), "host grok teardown"),
        tmux,
        target: exact_target(session),
        execution_id,
        session,
    };
    let result = settle_open_turn(
        deadline,
        GROK_TEARDOWN_POLL_INTERVAL,
        driver.interrupt_plan(),
        &mut transport,
    )
    .await;

    match result.status {
        GrokSettleStatus::Skipped => GrokSettleOutcome::Skipped,
        GrokSettleStatus::Completed { .. } => GrokSettleOutcome::Completed,
        GrokSettleStatus::TimedOut => {
            tracing::warn!(
                execution_id,
                session,
                turn_started = result.progress.events.started,
                turn_ended = result.progress.events.ended,
                turn_completed = result.progress.updates.completed,
                settle_window_secs = window.as_secs_f64(),
                reason = GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON,
                "grok teardown: turn_completed did not arrive before settle bound; proceeding with kill",
            );
            GrokSettleOutcome::TimedOut
        }
    }
}
