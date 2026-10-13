//! Bounded shutdown of the engine's tokio runtime.
//!
//! Dropping a multi-thread runtime blocks until every `spawn_blocking` task
//! returns. One blocking task wedged on a hung subprocess therefore kept a
//! process alive for hours *after* it had logged `engine shutdown complete`,
//! still holding the tmux `@boss_engine_owner` claim that the next engine's
//! adoption refuses to override. The engine shuts its runtime down through
//! [`shutdown_runtime`] instead, which gives blocking work a grace period and
//! then abandons it so `main` can return and the process exit.

use std::time::{Duration, Instant};

use tokio::runtime::Runtime;

/// How long blocking tasks get to finish once the engine has decided to stop.
pub const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// What [`shutdown_runtime`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeShutdown {
    /// Blocking tasks were still running at the deadline and were abandoned.
    pub abandoned_blocking_tasks: bool,
    /// Subprocesses the caller reaped because their tasks were abandoned.
    pub abandoned_commands: Vec<String>,
}

/// Shut `runtime` down, waiting at most `timeout` for blocking tasks.
///
/// When the deadline passes with work still running, `reap` is called to
/// terminate and name whatever that work was waiting on (subprocesses the
/// abandoned threads would otherwise leave behind), and the outcome is logged.
pub fn shutdown_runtime(runtime: Runtime, timeout: Duration, reap: impl FnOnce() -> Vec<String>) -> RuntimeShutdown {
    let started = Instant::now();
    runtime.shutdown_timeout(timeout);
    // `shutdown_timeout` returns `()` either way; running out the full grace
    // period is the only signal that blocking work was left behind.
    let abandoned_blocking_tasks = started.elapsed() >= timeout;
    if !abandoned_blocking_tasks {
        return RuntimeShutdown {
            abandoned_blocking_tasks,
            abandoned_commands: Vec::new(),
        };
    }
    let abandoned_commands = reap();
    tracing::warn!(
        timeout_secs = timeout.as_secs_f64(),
        abandoned_commands = ?abandoned_commands,
        "runtime shutdown timed out; abandoned blocking tasks still running so the process can exit"
    );
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
    fn a_clean_runtime_shuts_down_without_abandoning_anything() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.spawn_blocking(|| {});
        let outcome = shutdown_runtime(runtime, Duration::from_secs(30), || panic!("nothing to reap"));
        assert!(!outcome.abandoned_blocking_tasks);
        assert!(outcome.abandoned_commands.is_empty());
    }
}
