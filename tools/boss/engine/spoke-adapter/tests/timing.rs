//! Keep successful real-network operations independent of executor scheduling.
use std::future::Future;
use std::panic::Location;
use std::time::{Duration, Instant};

/// Tokio auto-advances a paused clock when no task is runnable, including while
/// real TCP traffic is in flight. Keep a runnable branch until the operation
/// completes; only explicit advances in the test can expire a command.
#[track_caller]
pub fn frozen<T>(operation: impl Future<Output = T>) -> impl Future<Output = T> {
    let armed_at = Location::caller();
    async move {
        tokio::time::pause();
        let result = guarded_at(armed_at, operation).await;
        tokio::time::resume();
        result
    }
}

/// A wall-clock hang guard, independent of the manually controlled Tokio clock.
/// Its panic names the call that armed it, so a hang says which stage of the
/// test stopped making progress.
#[track_caller]
pub fn guarded<T>(operation: impl Future<Output = T>) -> impl Future<Output = T> {
    guarded_at(Location::caller(), operation)
}

async fn guarded_at<T>(armed_at: &'static Location<'static>, operation: impl Future<Output = T>) -> T {
    let started = Instant::now();
    tokio::select! {
        result = operation => result,
        () = async {
            loop {
                assert!(
                    started.elapsed() < Duration::from_secs(120),
                    "test made no completion within two minutes (hang guard armed at {armed_at})"
                );
                tokio::task::yield_now().await;
            }
        } => unreachable!(),
    }
}

/// Observe persisted state without consuming any of the virtual deadline.
pub async fn condition(mut ready: impl FnMut() -> bool) {
    while !ready() {
        tokio::task::yield_now().await;
    }
}

/// Hold the Tokio clock paused for the rest of the test. Pair with a fixture
/// whose stream liveness timers cannot fire (`Fixture::with_frozen_liveness`)
/// so only the explicit advances below can expire a command deadline.
pub fn hold() {
    tokio::time::pause();
}

/// Real time each [`advancing`] step lets pass before it moves the clock.
const STEP_PACE: Duration = Duration::from_millis(1);

/// Advance the held clock in small steps until `ready` holds. Use it only
/// after the command that is deliberately left unanswered has been observed.
///
/// Every step first lets [`STEP_PACE`] of real time pass. The work under test
/// still runs on real time whatever the held clock says: SQLite calls on the
/// blocking pool, frames crossing the socket. Stepping as fast as the CPU can
/// spin would put minutes of virtual time between one of those calls and its
/// reply, causing a deadline the test never means to reach (the coordinator's
/// 90 s lease backstop) to fire while the reply is still in flight. Pacing
/// bounds virtual time at fifty times real time, whatever the speed or load of
/// the machine.
pub async fn advancing(mut ready: impl FnMut() -> bool) {
    while !ready() {
        // Blocking this thread is the point: it is the only thread of the
        // test's runtime, so the clock cannot move while the pause lasts.
        std::thread::sleep(STEP_PACE);
        tokio::time::advance(Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
    }
}
