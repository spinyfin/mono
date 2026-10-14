//! A contended synchronous DB mutex must not pin an async runtime worker.
use super::{Connection, Mutex, PooledConnection, Result};
use std::sync::TryLockError;

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
