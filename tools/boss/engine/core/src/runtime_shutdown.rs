//! Bounded shutdown of the engine's tokio runtime.
//!
//! In-flight preflight process groups are killed before the runtime grace
//! period so their blocking waits can finish. Any remaining blocking tasks
//! are abandoned at the deadline so the engine can exit and release its
//! tmux ownership claim.

use std::time::{Duration, Instant};

use tokio::runtime::Runtime;

/// How long blocking tasks get to finish once the engine has decided to stop.
pub const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// What [`shutdown_runtime`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeShutdown {
    /// Blocking tasks were still running at the deadline and were abandoned.
    pub abandoned_blocking_tasks: bool,
    /// In-flight subprocesses reaped before waiting for blocking tasks.
    pub abandoned_commands: Vec<String>,
}

/// Shut `runtime` down, waiting at most `timeout` for blocking tasks.
///
/// Reap and report in-flight commands first, allowing their blocking waits
/// to finish during the grace period. Report remaining tasks at the deadline.
pub fn shutdown_runtime(runtime: Runtime, timeout: Duration, reap: impl FnOnce() -> Vec<String>) -> RuntimeShutdown {
    let abandoned_commands = reap();
    if !abandoned_commands.is_empty() {
        tracing::warn!(
            abandoned_commands = ?abandoned_commands,
            "reaped in-flight preflight commands before runtime shutdown"
        );
    }
    let started = Instant::now();
    runtime.shutdown_timeout(timeout);
    // Tokio returns no completion status; elapsed time is a best-effort
    // indication that the grace period was exhausted.
    let abandoned_blocking_tasks = started.elapsed() >= timeout;
    if abandoned_blocking_tasks {
        tracing::warn!(
            timeout_secs = timeout.as_secs_f64(),
            "runtime shutdown timed out; abandoned blocking tasks still running so the process can exit"
        );
    }
    RuntimeShutdown {
        abandoned_blocking_tasks,
        abandoned_commands,
    }
}

/// [`shutdown_runtime`] for the engine: reaps any worker-preflight
/// subprocesses still in flight.
pub fn shutdown_engine_runtime(runtime: Runtime) -> RuntimeShutdown {
    shutdown_runtime(
        runtime,
        RUNTIME_SHUTDOWN_TIMEOUT,
        boss_engine_driver::grok::abandon_in_flight_preflight_commands,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A stuck blocking task must not keep shutdown from returning.
    #[test]
    fn shutdown_returns_despite_a_stuck_blocking_task() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (release, stuck) = mpsc::channel::<()>();
        let (started_tx, started_rx) = mpsc::channel::<()>();
        runtime.spawn_blocking(move || {
            started_tx.send(()).unwrap();
            // Blocks until the test releases it — i.e. "forever" for shutdown.
            let _ = stuck.recv();
        });
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("blocking task started");

        let began = Instant::now();
        let outcome = shutdown_runtime(runtime, Duration::from_millis(300), || {
            vec!["cube workspace status".to_owned()]
        });

        assert!(began.elapsed() < Duration::from_secs(30), "shutdown was not bounded");
        assert!(outcome.abandoned_blocking_tasks);
        assert_eq!(outcome.abandoned_commands, ["cube workspace status"]);
        drop(release);
    }

    #[test]
    fn reaping_releases_blocking_work_during_the_grace_period() {
        let runtime = tokio::runtime::Builder::new_multi_thread().build().unwrap();
        let (release, waiting) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        runtime.spawn_blocking(move || {
            started_tx.send(()).unwrap();
            waiting.recv().unwrap();
            finished_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();

        let outcome = shutdown_runtime(runtime, Duration::from_secs(10), || {
            release.send(()).unwrap();
            vec!["preflight command".to_owned()]
        });

        assert!(!outcome.abandoned_blocking_tasks);
        assert_eq!(outcome.abandoned_commands, ["preflight command"]);
        finished_rx
            .try_recv()
            .expect("blocking work finished before shutdown returned");
    }

    #[test]
    fn a_clean_runtime_shuts_down_without_abandoning_anything() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.spawn_blocking(|| {});
        let outcome = shutdown_runtime(runtime, Duration::from_secs(30), Vec::new);
        assert!(!outcome.abandoned_blocking_tasks);
        assert!(outcome.abandoned_commands.is_empty());
    }
}
