//! Keep successful real-network operations independent of executor scheduling.
use std::future::Future;
use std::time::{Duration, Instant};

/// Tokio auto-advances a paused clock when no task is runnable, including while
/// real TCP traffic is in flight. Keep a runnable branch until the operation
/// completes; only explicit advances in the test can expire a command.
pub async fn frozen<T>(operation: impl Future<Output = T>) -> T {
    tokio::time::pause();
    let result = guarded(operation).await;
    tokio::time::resume();
    result
}

/// A wall-clock hang guard, independent of the manually controlled Tokio clock.
pub async fn guarded<T>(operation: impl Future<Output = T>) -> T {
    let started = Instant::now();
    tokio::select! {
        result = operation => result,
        () = async {
            loop {
                assert!(started.elapsed() < Duration::from_secs(120), "test made no completion within two minutes");
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
