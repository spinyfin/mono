//! A contended synchronous DB mutex must not pin an async runtime worker.
//!
//! On a multithread runtime a contended `acquire` wraps the blocking
//! `Mutex::lock` in `block_in_place`, which hands the worker's core (and its
//! queued tasks, timers and IO) to another thread while this one sleeps.
//! Trade-offs, accepted deliberately:
//!
//! - Each contended caller can cost one extra thread from tokio's blocking
//!   pool (bounded by its `max_blocking_threads` cap, shared with
//!   `spawn_blocking` reads). A pile-up behind a slow holder therefore grows
//!   threads rather than starving the runtime; the slow holder is what the
//!   timing log below exists to name.
//! - `block_in_place` only frees the worker for *other* tasks. Futures
//!   multiplexed inside the same task (`select!`/`join!`) cannot progress
//!   while it waits, so long-running background DB callers belong on
//!   `spawn_blocking` rather than directly in an async task.
//! - The wait itself is unbounded: giving up would turn a slow query into a
//!   spurious failure of a caller that has no retry path. A wait or hold of
//!   at least 100 ms is logged with the caller's location instead.
//!
//! Synchronous callers and current-thread runtimes lock normally.
use super::{Connection, Mutex, PooledConnection, Result};
use std::sync::TryLockError;

// Lock acquisition cannot return SQLite errors. Keep this outside the result
// boundary so track_caller forwards the original call site without a closure.
impl super::WorkDb {
    /// Borrow the shared connection. Every call site gets what looks like
    /// its own `Connection` (via `Deref`/`DerefMut`) but is really a mutex
    /// guard over the one connection this `WorkDb` (and every clone of it)
    /// shares — see the docs on `WorkDb::conn`. A function that already
    /// holds a `connect()` guard must drop it (e.g. scope it in a block)
    /// before calling anything that connects again, or it deadlocks against
    /// itself.
    #[track_caller]
    pub(crate) fn connect(&self) -> Result<PooledConnection<'_>> {
        super::connection_wait::acquire(&self.conn)
    }
}

pub(crate) struct ConnectionGuard<'a> {
    guard: Option<std::sync::MutexGuard<'a, Connection>>,
    caller: &'static std::panic::Location<'static>,
    acquired: std::time::Instant,
    waited: std::time::Duration,
}

impl<'a> ConnectionGuard<'a> {
    fn new(
        guard: std::sync::MutexGuard<'a, Connection>,
        caller: &'static std::panic::Location<'static>,
        waited: std::time::Duration,
    ) -> Self {
        Self {
            guard: Some(guard),
            caller,
            acquired: std::time::Instant::now(),
            waited,
        }
    }
}

impl std::ops::Deref for ConnectionGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.guard.as_deref().expect("live connection guard")
    }
}

impl std::ops::DerefMut for ConnectionGuard<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.guard.as_deref_mut().expect("live connection guard")
    }
}

impl Drop for ConnectionGuard<'_> {
    fn drop(&mut self) {
        let held = self.acquired.elapsed();
        // Logging must not extend the critical section being measured.
        drop(self.guard.take());
        if held.max(self.waited) >= std::time::Duration::from_millis(100) {
            tracing::warn!(wait_ms = self.waited.as_millis() as u64, held_ms = held.as_millis() as u64, caller = %self.caller,
                "work DB connection lock timing");
            super::lock_stats::global().record_timing(self.caller, held);
        }
    }
}

#[track_caller]
pub(super) fn acquire(connection: &Mutex<Connection>) -> Result<PooledConnection<'_>> {
    let caller = std::panic::Location::caller();
    match connection.try_lock() {
        Ok(guard) => return Ok(ConnectionGuard::new(guard, caller, std::time::Duration::ZERO)),
        Err(TryLockError::Poisoned(_)) => anyhow::bail!("work db connection lock poisoned"),
        Err(TryLockError::WouldBlock) => {}
    }
    let start = std::time::Instant::now();
    let lock = || {
        connection
            .lock()
            .map_err(|_| anyhow::anyhow!("work db connection lock poisoned"))
    };
    // Production uses a multithread runtime. Hand its worker core back before
    // sleeping in std::Mutex so admission, timers and socket IO can progress.
    // Synchronous callers and current-thread test runtimes retain normal locking.
    let guard = match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(lock)
        }
        _ => lock(),
    };
    let waited = start.elapsed();
    guard.map(|guard| ConnectionGuard::new(guard, caller, waited))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::log_capture;
    use std::sync::Arc;
    use std::time::Duration;

    fn mutex() -> Mutex<Connection> {
        Mutex::new(Connection::open_in_memory().unwrap())
    }

    /// Log lines naming this file as the caller, emitted since `from`.
    fn lines_since(buffer: &log_capture::SharedBuffer, from: usize) -> Vec<String> {
        let log = String::from_utf8_lossy(&buffer.lock()[from..]).into_owned();
        log.lines()
            .filter(|line| line.contains("work DB connection lock timing") && line.contains("connection_wait.rs"))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn slow_waits_and_holds_are_logged_with_caller_and_fast_ones_are_not() {
        let buffer = log_capture::install();
        let from = buffer.lock().len();
        let fast = mutex();
        drop(acquire(&fast).unwrap());
        assert!(
            lines_since(&buffer, from).is_empty(),
            "fast uncontended lock must not log"
        );

        let from = buffer.lock().len();
        let held = acquire(&fast).unwrap();
        std::thread::sleep(Duration::from_millis(120));
        drop(held);
        let lines = lines_since(&buffer, from);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("wait_ms=0"), "{}", lines[0]);
        assert!(!lines[0].contains("held_ms=0 "), "{}", lines[0]);

        let from = buffer.lock().len();
        let contended = Arc::new(mutex());
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn({
            let contended = contended.clone();
            move || {
                let guard = contended.lock().unwrap();
                locked_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(150));
                drop(guard);
            }
        });
        locked_rx.recv().unwrap();
        drop(acquire(&contended).unwrap());
        holder.join().unwrap();
        let lines = lines_since(&buffer, from);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(!lines[0].contains("wait_ms=0 "), "{}", lines[0]);
    }
}
