//! Distinguish environmental interruptions from slow detection work.
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::time::Instant;

use crate::PassTrace;

const HEARTBEAT: Duration = Duration::from_secs(1);
const SLEEP_GAP: Duration = Duration::from_secs(10);

fn sleep_gap(wall: Duration, monotonic: Duration, tick: Duration, cadence: Duration) -> bool {
    wall.saturating_sub(monotonic) > SLEEP_GAP || tick > cadence.saturating_mul(3).max(SLEEP_GAP)
}

pub(crate) fn observe<T>(
    trace: &PassTrace,
    cadence: Duration,
    work: impl std::future::Future<Output = T>,
) -> impl std::future::Future<Output = T> {
    let wall = SystemTime::now();
    // Keep the engine's large pass future out of the observer's inline state.
    observe_with_clock(trace, cadence, Box::pin(work), move || {
        wall.elapsed().unwrap_or_default()
    })
}

async fn observe_with_clock<T>(
    trace: &PassTrace,
    cadence: Duration,
    work: impl std::future::Future<Output = T>,
    wall_elapsed: impl Fn() -> Duration,
) -> T {
    let unreachable = Arc::new(boss_github::availability::Observation::default());
    let observed = boss_github::availability::observe(unreachable.clone(), work);
    tokio::pin!(observed);
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
            wall_elapsed(),
            now.saturating_duration_since(started),
            now.saturating_duration_since(previous),
            cadence,
        );
        previous = now;
        if let Some(result) = result {
            if interrupted || unreachable.unreachable() {
                trace.interrupt();
                tracing::info!(
                    host_sleep = interrupted,
                    github_unreachable = unreachable.unreachable(),
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
        // Interruptions preserve an existing error without advancing its count.
        record_pass_result(&signal, false, &trace.timings(), Duration::from_secs(180), 1);
        record_pass_result(&signal, false, &trace.timings(), Duration::from_secs(180), 2);
        let previous = signal.borrow().clone();
        let result = observe(
            &trace,
            Duration::from_secs(60),
            tokio::time::timeout(Duration::from_secs(180), async {
                if sleep {
                    // No heartbeat is polled during the simulated suspension.
                    tokio::time::advance(Duration::from_secs(600)).await;
                } else {
                    use std::os::unix::process::ExitStatusExt;
                    boss_github::availability::observe_output(&Ok(std::process::Output {
                        status: std::process::ExitStatus::from_raw(256),
                        stdout: Vec::new(),
                        stderr: b"error connecting to api.github.com: no such host".to_vec(),
                    }));
                }
                std::future::pending::<()>().await;
            }),
        )
        .await;
        assert!(result.is_err());
        assert!(trace.timings().interrupted);
        for completed in [false, true] {
            record_pass_result(&signal, completed, &trace.timings(), Duration::from_secs(180), 3);
            assert_eq!(*signal.borrow(), previous);
            let healthy = detection_stall_signal();
            record_pass_result(&healthy, completed, &trace.timings(), Duration::from_secs(180), 3);
            assert!(healthy.borrow().is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn wall_clock_suspend_interrupts_without_heartbeat_gap() {
        let trace = PassTrace::default();
        let wall = std::cell::Cell::new(Duration::ZERO);
        observe_with_clock(
            &trace,
            Duration::from_secs(60),
            async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                wall.set(Duration::from_secs(600));
            },
            || wall.get(),
        )
        .await;
        assert!(trace.timings().interrupted);
    }

    #[tokio::test(start_paused = true)]
    async fn suspended_pass_preserves_stall() {
        run_interrupted_pass(true).await;
    }

    #[tokio::test(start_paused = true)]
    async fn transport_error_preserves_stall() {
        run_interrupted_pass(false).await;
    }

    #[tokio::test(start_paused = true)]
    async fn consecutive_slow_passes_still_raise_an_error() {
        use crate::pass_trace::{detection_stall_health_issue, detection_stall_signal, record_pass_result};
        let signal = detection_stall_signal();
        for now in [180, 360] {
            let trace = PassTrace::default();
            trace.begin();
            let metrics = crate::metrics::Registry::new();
            crate::init(&metrics);
            let result = crate::schedule::run_pass_within_budget(
                "test",
                Duration::from_secs(180),
                Duration::from_secs(60),
                &metrics,
                &trace,
                async {
                    let deadline = crate::probe_budget::deadline();
                    for _ in 0..2 {
                        crate::probe_budget::bounded(deadline, tokio::time::sleep(Duration::from_secs(14)))
                            .await
                            .unwrap();
                        use std::os::unix::process::ExitStatusExt;
                        boss_github::availability::observe_output(&Ok(std::process::Output {
                            status: std::process::ExitStatus::from_raw(0),
                            stdout: b"{}".to_vec(),
                            stderr: Vec::new(),
                        }));
                    }
                    assert!(
                        crate::probe_budget::bounded(deadline, tokio::time::sleep(Duration::from_secs(14)))
                            .await
                            .is_err()
                    );
                    std::future::pending::<()>().await;
                },
            )
            .await;
            assert!(result.is_none());
            assert!(!trace.timings().interrupted);
            record_pass_result(&signal, false, &trace.timings(), Duration::from_secs(180), now);
        }
        let stall = signal.borrow().clone().unwrap();
        assert_eq!(stall.consecutive_abandoned, 2);
        assert_eq!(detection_stall_health_issue(&stall, 360).severity, "error");
    }
}
