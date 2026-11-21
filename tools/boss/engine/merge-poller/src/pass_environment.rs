//! Distinguish environmental interruptions from slow detection work.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime};
use tokio::time::Instant;

use crate::PassTrace;

const HEARTBEAT: Duration = Duration::from_secs(1);
const SLEEP_GAP: Duration = Duration::from_secs(10);

fn sleep_gap(wall: Duration, monotonic: Duration, tick: Duration, cadence: Duration) -> bool {
    wall.saturating_sub(monotonic) > SLEEP_GAP || tick > cadence.saturating_mul(3).max(SLEEP_GAP)
}

pub(crate) async fn observe<T>(trace: &PassTrace, cadence: Duration, work: impl std::future::Future<Output = T>) -> T {
    let unreachable = Arc::new(AtomicBool::new(false));
    let observed = boss_github::availability::observe(unreachable.clone(), work);
    tokio::pin!(observed);
    let wall = SystemTime::now();
    let started = Instant::now();
    let mut previous = started;
    let mut interrupted = false;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let result = tokio::select! {
            result = &mut observed => Some(result),
            _ = heartbeat.tick() => None,
        };
        let now = Instant::now();
        interrupted |= sleep_gap(
            wall.elapsed().unwrap_or_default(),
            now.saturating_duration_since(started),
            now.saturating_duration_since(previous),
            cadence,
        );
        previous = now;
        if let Some(result) = result {
            if interrupted || unreachable.load(Ordering::Relaxed) {
                trace.interrupt();
                tracing::info!(
                    host_sleep = interrupted,
                    github_unreachable = unreachable.load(Ordering::Relaxed),
                    "merge poller: environmental interruption; excluding pass from detection stall health"
                );
            }
            return result;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sleep_on_clocks_that_pause_or_advance_is_detected() {
        assert!(sleep_gap(
            Duration::from_secs(600),
            Duration::from_secs(1),
            HEARTBEAT,
            Duration::from_secs(60)
        ));
        assert!(sleep_gap(
            Duration::from_secs(600),
            Duration::from_secs(600),
            Duration::from_secs(600),
            Duration::from_secs(60),
        ));
        assert!(!sleep_gap(
            Duration::from_secs(600),
            Duration::from_secs(600),
            HEARTBEAT,
            Duration::from_secs(60),
        ));
    }

    async fn run_interrupted_pass(sleep: bool) {
        use crate::pass_trace::{detection_stall_signal, record_pass_result};
        let trace = PassTrace::default();
        trace.begin();
        let signal = detection_stall_signal();
        // An environmental interruption also withdraws an earlier alarm.
        record_pass_result(&signal, false, &trace.timings(), Duration::from_secs(180), 1);
        assert!(signal.borrow().is_some());
        let result = observe(
            &trace,
            Duration::from_secs(60),
            tokio::time::timeout(Duration::from_secs(180), async {
                if sleep {
                    // No heartbeat is polled during the simulated suspension.
                    tokio::time::advance(Duration::from_secs(600)).await;
                } else {
                    let result = crate::probe_budget::bounded(
                        Instant::now() + Duration::from_secs(30),
                        std::future::pending::<()>(),
                    )
                    .await;
                    assert!(result.is_err());
                }
                std::future::pending::<()>().await;
            }),
        )
        .await;
        assert!(result.is_err());
        assert!(trace.timings().interrupted);
        record_pass_result(&signal, false, &trace.timings(), Duration::from_secs(180), 2);
        assert!(signal.borrow().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn suspended_pass_does_not_raise_a_stall() {
        run_interrupted_pass(true).await;
    }

    #[tokio::test(start_paused = true)]
    async fn transport_timeout_does_not_raise_a_stall() {
        run_interrupted_pass(false).await;
    }

    #[tokio::test(start_paused = true)]
    async fn consecutive_slow_passes_still_raise_an_error() {
        use crate::pass_trace::{detection_stall_health_issue, detection_stall_signal, record_pass_result};
        let signal = detection_stall_signal();
        for now in [180, 360] {
            let trace = PassTrace::default();
            trace.begin();
            let result = observe(
                &trace,
                Duration::from_secs(60),
                tokio::time::timeout(Duration::from_secs(180), std::future::pending::<()>()),
            )
            .await;
            assert!(result.is_err());
            assert!(!trace.timings().interrupted);
            record_pass_result(&signal, false, &trace.timings(), Duration::from_secs(180), now);
        }
        let stall = signal.borrow().clone().unwrap();
        assert_eq!(stall.consecutive_abandoned, 2);
        assert_eq!(detection_stall_health_issue(&stall, 360).severity, "error");
    }
}
