//! Bounded Grok ACP cancel-and-wait before a host-owned `kill_session_verified`.
//!
//! Spoke Grok workers are stopped here, not by the engine's local tmux reap.
//! An open turn must be cancelled with the driver's interrupt plan and given a
//! short window to emit `turn_completed` into `updates.jsonl` so the spoke
//! transcript observer can journal usage before the session is destroyed.

use super::exact_target;
use boss_engine_driver::{DriverRegistry, InterruptPlan};
use boss_tmux::Tmux;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Upper bound on waiting for Grok to emit `turn_completed` after cancelling
/// an open turn. Kept short so host stop stays prompt if Grok never responds.
pub const GROK_TEARDOWN_SETTLE_WINDOW: Duration = Duration::from_secs(5);

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Reason returned on the Stop command when `turn_completed` never arrives.
pub const GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON: &str = "turn ended by teardown before turn_completed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrokSettleOutcome {
    Skipped,
    Completed,
    TimedOut,
}

impl GrokSettleOutcome {
    pub(crate) fn command_reason(self) -> String {
        match self {
            Self::TimedOut => GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON.to_owned(),
            Self::Skipped | Self::Completed => String::new(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct EventsTurnState {
    started: usize,
    ended: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct UpdatesTurnState {
    completed: usize,
    has_activity: bool,
}

struct JsonlCursor {
    path: PathBuf,
    offset: usize,
}

impl JsonlCursor {
    fn poll(&mut self) -> Vec<Value> {
        let Ok(bytes) = std::fs::read(&self.path) else {
            return Vec::new();
        };
        if bytes.len() < self.offset {
            self.offset = 0;
        }
        let slice = &bytes[self.offset..];
        let Some(last_newline) = slice.iter().rposition(|&b| b == b'\n') else {
            return Vec::new();
        };
        let complete = &slice[..=last_newline];
        self.offset += last_newline + 1;
        String::from_utf8_lossy(complete)
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                (!trimmed.is_empty()).then(|| serde_json::from_str::<Value>(trimmed).ok())?
            })
            .collect()
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
    _execution_id: &str,
    driver_id: &str,
) -> GrokSettleOutcome {
    if driver_id != boss_engine_driver::grok::TRANSCRIPT_DRIVER_SLUG {
        return GrokSettleOutcome::Skipped;
    }
    let Ok(driver) = DriverRegistry::default().require(driver_id) else {
        return GrokSettleOutcome::Skipped;
    };
    let window = settle_window();
    let deadline = tokio::time::Instant::now() + window;
    let Some(home) = grok_home else {
        return GrokSettleOutcome::Skipped;
    };
    let Some((events_path, updates_path)) = boss_engine_driver::grok::session_files_for_home(home) else {
        return GrokSettleOutcome::Skipped;
    };
    let mut events = JsonlCursor {
        path: events_path,
        offset: 0,
    };
    let mut updates = JsonlCursor {
        path: updates_path,
        offset: 0,
    };
    let mut events_state = EventsTurnState::default();
    let mut updates_state = UpdatesTurnState::default();
    if timeout_remaining(deadline, async {
        fold_events(&mut events_state, events.poll());
        fold_updates(&mut updates_state, updates.poll());
    })
    .await
    .is_none()
    {
        return GrokSettleOutcome::TimedOut;
    }
    if events_state.started == 0 && !updates_state.has_activity {
        return GrokSettleOutcome::Skipped;
    }
    let expected_completed = events_state.started.max(1);
    if updates_state.completed >= expected_completed {
        return GrokSettleOutcome::Completed;
    }
    let turn_still_open =
        events_state.started > events_state.ended || (events_state.started == 0 && updates_state.has_activity);
    if turn_still_open {
        match timeout_remaining(deadline, send_interrupt_gesture(tmux, session, driver.interrupt_plan())).await {
            Some(Ok(())) => {}
            Some(Err(_)) | None => return GrokSettleOutcome::TimedOut,
        }
    }
    loop {
        if remaining_budget(deadline).is_zero() {
            break;
        }
        if timeout_remaining(deadline, async {
            fold_events(&mut events_state, events.poll());
            fold_updates(&mut updates_state, updates.poll());
        })
        .await
        .is_none()
        {
            break;
        }
        if updates_state.completed >= expected_completed {
            return GrokSettleOutcome::Completed;
        }
        let sleep_for = POLL_INTERVAL.min(remaining_budget(deadline));
        if sleep_for.is_zero() {
            break;
        }
        tokio::time::sleep(sleep_for).await;
    }
    GrokSettleOutcome::TimedOut
}

async fn send_interrupt_gesture(tmux: &Tmux, session: &str, plan: Option<InterruptPlan>) -> anyhow::Result<()> {
    let key = plan.map(|plan| plan.gesture.key).unwrap_or("Escape");
    let presses = plan.map(|plan| plan.gesture.presses.max(1)).unwrap_or(1);
    let press_interval = plan.map(|plan| plan.gesture.press_interval).unwrap_or(Duration::ZERO);
    let target = exact_target(session);
    for press in 0..presses {
        if press > 0 && !press_interval.is_zero() {
            tokio::time::sleep(press_interval).await;
        }
        tmux.send_key(&target, key).await?;
    }
    Ok(())
}

fn remaining_budget(deadline: tokio::time::Instant) -> Duration {
    deadline.saturating_duration_since(tokio::time::Instant::now())
}

async fn timeout_remaining<F, T>(deadline: tokio::time::Instant, fut: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    let budget = remaining_budget(deadline);
    if budget.is_zero() {
        return None;
    }
    tokio::time::timeout(budget, fut).await.ok()
}

fn fold_events(state: &mut EventsTurnState, values: Vec<Value>) {
    for value in values {
        if boss_engine_driver::grok::is_turn_started(&value) {
            state.started += 1;
        } else if boss_engine_driver::grok::is_turn_end(&value) {
            state.ended += 1;
        }
    }
}

fn fold_updates(state: &mut UpdatesTurnState, values: Vec<Value>) {
    for value in values {
        state.has_activity = true;
        if boss_engine_driver::grok::is_turn_completed(&value) {
            state.completed += 1;
        }
    }
}
