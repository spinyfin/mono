//! Shared Grok ACP pre-reap turn-settlement policy.
//!
//! Both local (`boss_engine_app`) and spoke (`boss_host_execution`) Grok
//! teardowns share the same turn-state fold, settle decision, interrupt
//! gesture sequencing, and bounded poll loop. Consumers supply only their
//! transcript tailing, tmux `send-keys` transport, and persistence/projection.

use std::future::Future;
use std::time::Duration;

use serde_json::Value;

use super::{is_turn_completed, is_turn_end, is_turn_started};
use crate::InterruptPlan;

/// Upper bound on waiting for Grok to emit `turn_completed` after cancelling an
/// open turn before teardown proceeds. Kept short so slot, host, and lease
/// release stay prompt even if Grok never responds.
pub const GROK_TEARDOWN_SETTLE_WINDOW: Duration = Duration::from_secs(5);

/// Poll cadence while waiting for `turn_completed` to land.
pub const GROK_TEARDOWN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Specific `usage_unavailable` reason persisted when a Grok turn is still
/// missing `turn_completed` after the pre-reap settle bound expires.
pub const GROK_TEARDOWN_USAGE_UNAVAILABLE_REASON: &str = "turn ended by teardown before turn_completed";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EventsTurnState {
    pub started: usize,
    pub ended: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UpdatesTurnState {
    pub completed: usize,
    pub has_activity: bool,
}

/// Accumulated turn-boundary state from `events.jsonl` and `updates.jsonl`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GrokTurnProgress {
    pub events: EventsTurnState,
    pub updates: UpdatesTurnState,
}

/// What the settle policy decides after the initial tail poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokSettleDecision {
    /// Neither `events.jsonl` nor `updates.jsonl` shows any turn activity.
    Skip,
    /// Every started turn already has a matching `turn_completed` in `updates.jsonl`.
    AlreadyComplete { completed: usize },
    /// A turn is still open; send the interrupt gesture and wait for `turn_completed`.
    CancelAndWait { expected_completed: usize },
    /// The turn already ended in `events.jsonl` without `turn_completed`; wait without sending keys.
    WaitOnly { expected_completed: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokSettleStatus {
    Skipped,
    Completed { completed: usize, already_complete: bool },
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrokSettleResult {
    pub status: GrokSettleStatus,
    pub progress: GrokTurnProgress,
}

impl GrokTurnProgress {
    pub fn reset_events(&mut self) {
        self.events = EventsTurnState::default();
    }

    pub fn reset_updates(&mut self) {
        self.updates = UpdatesTurnState::default();
    }

    pub fn fold_events(&mut self, values: impl IntoIterator<Item = Value>) {
        for value in values {
            if is_turn_started(&value) {
                self.events.started += 1;
            } else if is_turn_end(&value) {
                self.events.ended += 1;
            }
        }
    }

    pub fn fold_updates(&mut self, values: impl IntoIterator<Item = Value>) {
        for value in values {
            self.updates.has_activity = true;
            if is_turn_completed(&value) {
                self.updates.completed += 1;
            }
        }
    }

    pub fn expected_completed(&self) -> usize {
        self.events.started.max(1)
    }

    pub fn turn_still_open(&self) -> bool {
        self.events.started > self.events.ended || (self.events.started == 0 && self.updates.has_activity)
    }

    pub fn decision(&self) -> GrokSettleDecision {
        if self.events.started == 0 && !self.updates.has_activity {
            return GrokSettleDecision::Skip;
        }
        let expected_completed = self.expected_completed();
        if self.updates.completed >= expected_completed {
            return GrokSettleDecision::AlreadyComplete {
                completed: self.updates.completed,
            };
        }
        if self.turn_still_open() {
            GrokSettleDecision::CancelAndWait { expected_completed }
        } else {
            GrokSettleDecision::WaitOnly { expected_completed }
        }
    }
}

pub fn remaining_budget(deadline: tokio::time::Instant) -> Duration {
    deadline.saturating_duration_since(tokio::time::Instant::now())
}

pub async fn timeout_remaining<F, T>(deadline: tokio::time::Instant, fut: F) -> Option<T>
where
    F: Future<Output = T>,
{
    let budget = remaining_budget(deadline);
    if budget.is_zero() {
        return None;
    }
    tokio::time::timeout(budget, fut).await.ok()
}

/// Transport and observability hooks for [`settle_open_turn`].
#[async_trait::async_trait]
pub trait GrokSettleTransport {
    type Error;

    async fn poll(&mut self, progress: &mut GrokTurnProgress);
    async fn send_key(&mut self, key: &'static str) -> Result<(), Self::Error>;

    fn on_cancel_start(&mut self, _progress: &GrokTurnProgress) {}
    fn on_send_error(&mut self, _error: Self::Error) {}
}

/// Execute the driver's interrupt gesture sequence (falling back to one `Escape`
/// press when no plan is supplied) using the caller's key-sending transport.
async fn send_interrupt_gesture_via<T: GrokSettleTransport + ?Sized>(
    plan: Option<InterruptPlan>,
    transport: &mut T,
) -> Result<(), T::Error> {
    let key = plan.map(|p| p.gesture.key).unwrap_or("Escape");
    let presses = plan.map(|p| p.gesture.presses.max(1)).unwrap_or(1);
    let press_interval = plan.map(|p| p.gesture.press_interval).unwrap_or(Duration::ZERO);
    for press in 0..presses {
        if press > 0 && !press_interval.is_zero() {
            tokio::time::sleep(press_interval).await;
        }
        transport.send_key(key).await?;
    }
    Ok(())
}

/// Bounded Grok ACP settle state machine over caller-supplied [`GrokSettleTransport`].
pub async fn settle_open_turn<T: GrokSettleTransport + ?Sized>(
    deadline: tokio::time::Instant,
    poll_interval: Duration,
    plan: Option<InterruptPlan>,
    transport: &mut T,
) -> GrokSettleResult {
    let mut progress = GrokTurnProgress::default();
    if timeout_remaining(deadline, transport.poll(&mut progress))
        .await
        .is_none()
    {
        return GrokSettleResult {
            status: GrokSettleStatus::TimedOut,
            progress,
        };
    }

    let expected_completed = match progress.decision() {
        GrokSettleDecision::Skip => {
            return GrokSettleResult {
                status: GrokSettleStatus::Skipped,
                progress,
            };
        }
        GrokSettleDecision::AlreadyComplete { completed } => {
            return GrokSettleResult {
                status: GrokSettleStatus::Completed {
                    completed,
                    already_complete: true,
                },
                progress,
            };
        }
        GrokSettleDecision::CancelAndWait { expected_completed } => {
            transport.on_cancel_start(&progress);
            match timeout_remaining(deadline, send_interrupt_gesture_via(plan, transport)).await {
                Some(Ok(())) => Some(expected_completed),
                Some(Err(err)) => {
                    transport.on_send_error(err);
                    None
                }
                None => {
                    return GrokSettleResult {
                        status: GrokSettleStatus::TimedOut,
                        progress,
                    };
                }
            }
        }
        GrokSettleDecision::WaitOnly { expected_completed } => Some(expected_completed),
    };

    if let Some(expected) = expected_completed {
        loop {
            if remaining_budget(deadline).is_zero() {
                break;
            }
            if timeout_remaining(deadline, transport.poll(&mut progress))
                .await
                .is_none()
            {
                break;
            }
            if progress.updates.completed >= expected {
                return GrokSettleResult {
                    status: GrokSettleStatus::Completed {
                        completed: progress.updates.completed,
                        already_complete: false,
                    },
                    progress,
                };
            }
            let sleep_for = poll_interval.min(remaining_budget(deadline));
            if sleep_for.is_zero() {
                break;
            }
            tokio::time::sleep(sleep_for).await;
        }
    }

    let final_expected = progress.expected_completed();
    if timeout_remaining(deadline, transport.poll(&mut progress))
        .await
        .is_some()
        && progress.updates.completed >= final_expected
    {
        return GrokSettleResult {
            status: GrokSettleStatus::Completed {
                completed: progress.updates.completed,
                already_complete: false,
            },
            progress,
        };
    }

    GrokSettleResult {
        status: GrokSettleStatus::TimedOut,
        progress,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    fn turn_started() -> Value {
        json!({"type": "turn_started", "session_id": "s1", "turn_number": 0})
    }

    fn turn_ended() -> Value {
        json!({"type": "turn_ended", "outcome": "cancelled"})
    }

    fn update_activity() -> Value {
        json!({"method": "session/update", "params": {"update": {"sessionUpdate": "tool_call"}}})
    }

    fn turn_completed() -> Value {
        json!({"method": "_x.ai/session/update", "params": {"update": {"sessionUpdate": "turn_completed", "prompt_id": "p1"}}})
    }

    enum TestScenario {
        Skip,
        AlreadyComplete,
        CancelThenComplete,
        StalledPoll,
    }

    struct FakeTransport {
        scenario: TestScenario,
        polls: usize,
        keys_sent: usize,
    }

    impl FakeTransport {
        fn new(scenario: TestScenario) -> Self {
            Self {
                scenario,
                polls: 0,
                keys_sent: 0,
            }
        }
    }

    #[async_trait::async_trait]
    impl GrokSettleTransport for FakeTransport {
        type Error = ();

        async fn poll(&mut self, progress: &mut GrokTurnProgress) {
            self.polls += 1;
            match self.scenario {
                TestScenario::Skip => {}
                TestScenario::AlreadyComplete => {
                    if self.polls == 1 {
                        progress.fold_events([turn_started()]);
                        progress.fold_updates([turn_completed()]);
                    }
                }
                TestScenario::CancelThenComplete => {
                    if self.polls == 1 {
                        progress.fold_events([turn_started()]);
                        progress.fold_updates([update_activity()]);
                    } else if self.keys_sent > 0 && progress.updates.completed == 0 {
                        progress.fold_events([turn_ended()]);
                        progress.fold_updates([turn_completed()]);
                    }
                }
                TestScenario::StalledPoll => std::future::pending::<()>().await,
            }
        }

        async fn send_key(&mut self, _key: &'static str) -> Result<(), Self::Error> {
            self.keys_sent += 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn skips_when_no_activity_observed() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut transport = FakeTransport::new(TestScenario::Skip);
        let result = settle_open_turn(deadline, Duration::from_millis(10), None, &mut transport).await;
        assert_eq!(result.status, GrokSettleStatus::Skipped);
        assert_eq!(transport.keys_sent, 0);
    }

    #[tokio::test]
    async fn completes_immediately_when_already_complete() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut transport = FakeTransport::new(TestScenario::AlreadyComplete);
        let result = settle_open_turn(deadline, Duration::from_millis(10), None, &mut transport).await;
        assert_eq!(
            result.status,
            GrokSettleStatus::Completed {
                completed: 1,
                already_complete: true
            }
        );
        assert_eq!(transport.keys_sent, 0);
    }

    #[tokio::test]
    async fn cancels_open_turn_and_waits_for_turn_completed() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut transport = FakeTransport::new(TestScenario::CancelThenComplete);
        let result = settle_open_turn(deadline, Duration::from_millis(10), None, &mut transport).await;
        assert_eq!(
            result.status,
            GrokSettleStatus::Completed {
                completed: 1,
                already_complete: false
            }
        );
        assert_eq!(transport.keys_sent, 1);
    }

    #[tokio::test]
    async fn stalled_poll_times_out_within_deadline() {
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(60);
        let mut transport = FakeTransport::new(TestScenario::StalledPoll);
        let result = settle_open_turn(deadline, Duration::from_millis(10), None, &mut transport).await;
        let elapsed = started.elapsed();
        assert_eq!(result.status, GrokSettleStatus::TimedOut);
        assert!(
            elapsed < Duration::from_millis(400),
            "stalled poll must be bounded by deadline (elapsed={elapsed:?})"
        );
    }
}
