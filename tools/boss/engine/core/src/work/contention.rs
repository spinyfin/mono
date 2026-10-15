//! Transient SQLite lock contention, told apart from real database failures
//! in one place, and absorbed instead of returned to clients.
//!
//! SQLite reports a lock it could not take as `SQLITE_BUSY` (another
//! connection, possibly in another process, holds the lock) or
//! `SQLITE_LOCKED` (a conflict inside one process's shared cache). Neither is
//! something a client can act on: the right response is to wait and try
//! again. Until this module existed the engine's connections used a fixed
//! `busy_timeout`, after which the statement failed and the text "database
//! is locked" travelled to the macOS app as a modal and to the CLI as exit 6.
//!
//! The busy handler installed by [`install`] replaces that timeout. It
//! retries the lock acquisition with SQLite's own backoff schedule and does
//! not give up: a wait that never clears is reported through the engine
//! health surface ([`snapshot`]) as an engine problem, not through the
//! failing operation. Because the handler runs *inside* the statement that
//! could not take the lock, the enclosing transaction is never rolled back
//! and retried from outside — for a `BEGIN IMMEDIATE` or autocommit
//! statement the lock is taken before any row is touched, so a retried
//! write is applied exactly once.
//!
//! Every wait is logged with the caller that holds the connection, the
//! frontend request kind being served (when known), the attempt count and
//! the total time waited, so contention stays visible to operators even
//! though it no longer surfaces to users.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode};

/// A SQLite failure that is transient lock contention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Contention {
    pub(crate) code: ErrorCode,
    pub(crate) extended_code: i32,
}

/// Classify one rusqlite error. `SQLITE_BUSY` and `SQLITE_LOCKED` — with
/// every extended code (`BUSY_RECOVERY`, `BUSY_SNAPSHOT`, `BUSY_TIMEOUT`,
/// `LOCKED_SHAREDCACHE`, …) — are contention; everything else is a real
/// database failure.
pub(crate) fn classify_sqlite(err: &rusqlite::Error) -> Option<Contention> {
    match err {
        rusqlite::Error::SqliteFailure(ffi, _)
            if matches!(ffi.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) =>
        {
            Some(Contention {
                code: ffi.code,
                extended_code: ffi.extended_code,
            })
        }
        _ => None,
    }
}

/// Classify an `anyhow` error by the rusqlite error anywhere in its chain.
pub(crate) fn classify(err: &anyhow::Error) -> Option<Contention> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<rusqlite::Error>().and_then(classify_sqlite))
}

/// Install the absorbing busy handler on `conn`, replacing any
/// `busy_timeout`. Every connection the engine configures gets this.
pub(crate) fn install(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_handler(Some(absorb))
}

/// A wait longer than this is an engine-health problem. It is the
/// `busy_timeout` the engine ran with before contention was absorbed: every
/// wait past it used to reach the user as "database is locked", so it is the
/// empirically established point at which contention was already
/// user-visible. Engine write transactions hold the lock for milliseconds
/// (see the PR that introduced this module for the measurement), so a wait
/// this long means another holder is stuck, not that the engine is busy.
pub(crate) const HEALTH_THRESHOLD: Duration = Duration::from_secs(5);

/// First progress line for a wait still in flight, then every
/// `PROGRESS_INTERVAL` after it, so a wait that never clears keeps logging.
const PROGRESS_FIRST: Duration = Duration::from_secs(1);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// SQLite's own `sqliteDefaultBusyCallback` schedule (ms), then a steady
/// 100 ms: short enough that a lock released in microseconds is retried
/// promptly, long enough that a stuck lock costs no CPU.
const BACKOFF_MS: [u64; 12] = [1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100];

tokio::task_local! {
    /// The frontend request kind being served by the current task, set by
    /// the dispatch loop so the DB layer can name it in contention logs.
    pub(crate) static REQUEST_KIND: Arc<str>;
}

/// Request kind for the current task, or `"internal"` outside a frontend
/// dispatch (sweeps, pollers, and handlers that moved their DB call to a
/// separate blocking task).
pub(crate) fn current_request_kind() -> Arc<str> {
    REQUEST_KIND
        .try_with(|kind| kind.clone())
        .unwrap_or_else(|_| Arc::from("internal"))
}

/// Who holds the connection on this thread: pushed by the connection guard
/// so the busy handler, which SQLite calls with no context, can name it and
/// report to the right database's health registry.
#[derive(Clone, Debug)]
pub(crate) struct Holder {
    pub(crate) caller: &'static std::panic::Location<'static>,
    pub(crate) request: Arc<str>,
    pub(crate) health: Arc<Health>,
}

/// One lock wait in flight on this thread.
struct Wait {
    started: Instant,
    attempts: u32,
    next_progress: Duration,
    holder: Holder,
    /// Set once the wait is past [`HEALTH_THRESHOLD`] and registered as
    /// stuck, to the id it is registered under.
    reported: Option<u64>,
}

thread_local! {
    static HOLDERS: RefCell<Vec<Holder>> = const { RefCell::new(Vec::new()) };
    static WAIT: RefCell<Option<Wait>> = const { RefCell::new(None) };
}

static NEXT_WAIT_ID: AtomicU64 = AtomicU64::new(0);

/// Scope of one connection guard: names the holder for waits on this
/// thread and flushes the summary of an in-flight wait when it ends.
pub(crate) struct HolderScope;

impl HolderScope {
    pub(crate) fn enter(holder: Holder) -> Self {
        HOLDERS.with(|holders| holders.borrow_mut().push(holder));
        Self
    }
}

impl Drop for HolderScope {
    fn drop(&mut self) {
        finish_wait();
        HOLDERS.with(|holders| {
            holders.borrow_mut().pop();
        });
    }
}

/// The innermost guard on this thread, or — for a raw connection used
/// without one (`WorkDb::connect_new`) — an anonymous holder reporting to
/// the process-wide fallback registry.
fn current_holder() -> Holder {
    HOLDERS
        .with(|holders| holders.borrow().last().cloned())
        .unwrap_or_else(|| Holder {
            caller: std::panic::Location::caller(),
            request: Arc::from("unknown"),
            health: fallback_health(),
        })
}

/// The busy handler. `attempt` is SQLite's count of retries for the
/// current lock acquisition, starting at 0; returning `true` asks SQLite to
/// try again. A new acquisition (attempt 0) ends any previous wait on this
/// thread, which is the only signal SQLite gives that a wait cleared short
/// of the guard being dropped.
fn absorb(attempt: i32) -> bool {
    if attempt == 0 {
        finish_wait();
        let holder = current_holder();
        WAIT.with(|wait| {
            *wait.borrow_mut() = Some(Wait {
                started: Instant::now(),
                attempts: 0,
                next_progress: PROGRESS_FIRST,
                holder,
                reported: None,
            })
        });
    }
    let delay = WAIT.with(|wait| {
        let mut wait = wait.borrow_mut();
        let Some(wait) = wait.as_mut() else {
            return Duration::from_millis(100);
        };
        wait.attempts = wait.attempts.saturating_add(1);
        let waited = wait.started.elapsed();
        if waited >= wait.next_progress {
            wait.next_progress = waited + PROGRESS_INTERVAL;
            tracing::warn!(
                caller = %wait.holder.caller,
                request = %wait.holder.request,
                attempts = wait.attempts,
                waited_ms = waited.as_millis() as u64,
                "work DB contention: still waiting for the SQLite lock"
            );
        }
        if waited >= HEALTH_THRESHOLD {
            let id = *wait
                .reported
                .get_or_insert_with(|| NEXT_WAIT_ID.fetch_add(1, Ordering::Relaxed));
            wait.holder.health.stuck(id, &wait.holder, wait.started, wait.attempts);
        }
        Duration::from_millis(
            BACKOFF_MS
                .get(usize::try_from(attempt).unwrap_or(usize::MAX).min(BACKOFF_MS.len()))
                .copied()
                .unwrap_or(100),
        )
    });
    sleep_yielding(delay);
    true
}

/// End the wait in flight on this thread, logging its summary.
fn finish_wait() {
    let Some(wait) = WAIT.with(|wait| wait.borrow_mut().take()) else {
        return;
    };
    let waited = wait.started.elapsed();
    if let Some(id) = wait.reported {
        wait.holder.health.cleared(id);
    }
    wait.holder.health.record(waited);
    tracing::warn!(
        caller = %wait.holder.caller,
        request = %wait.holder.request,
        attempts = wait.attempts,
        waited_ms = waited.as_millis() as u64,
        "work DB contention: SQLite lock wait cleared"
    );
}

/// Sleep without pinning an async runtime worker: on the multithread
/// runtime the worker's core is handed to another thread for the duration,
/// mirroring `connection_wait::acquire`. Other threads just sleep.
fn sleep_yielding(delay: Duration) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| std::thread::sleep(delay))
        }
        _ => std::thread::sleep(delay),
    }
}

/// One database's view of its contention, for the health surface. Shared
/// by every `WorkDb` handle on that database (including the per-query
/// connections `query_connection` hands out). Counters cover the handle's
/// lifetime; `stuck` holds only waits currently past [`HEALTH_THRESHOLD`].
#[derive(Debug, Default)]
pub(crate) struct Health {
    waits: AtomicU64,
    long_waits: AtomicU64,
    longest_ms: AtomicU64,
    stuck: Mutex<BTreeMap<u64, StuckWait>>,
}

#[derive(Clone, Debug)]
pub(crate) struct StuckWait {
    pub(crate) caller: String,
    pub(crate) request: String,
    pub(crate) since: Instant,
    pub(crate) attempts: u32,
}

/// Point-in-time copy for the health report.
#[derive(Clone, Debug, Default)]
pub(crate) struct Snapshot {
    pub(crate) waits: u64,
    pub(crate) long_waits: u64,
    pub(crate) longest_ms: u64,
    pub(crate) stuck: Vec<StuckWait>,
}

impl Health {
    fn record(&self, waited: Duration) {
        let ms = waited.as_millis() as u64;
        self.waits.fetch_add(1, Ordering::Relaxed);
        if waited >= HEALTH_THRESHOLD {
            self.long_waits.fetch_add(1, Ordering::Relaxed);
        }
        self.longest_ms.fetch_max(ms, Ordering::Relaxed);
    }

    fn stuck(&self, id: u64, holder: &Holder, since: Instant, attempts: u32) {
        let mut stuck = self.stuck.lock().expect("contention health lock poisoned");
        let entry = stuck.entry(id).or_insert_with(|| StuckWait {
            caller: holder.caller.to_string(),
            request: holder.request.to_string(),
            since,
            attempts,
        });
        entry.attempts = attempts;
    }

    fn cleared(&self, id: u64) {
        self.stuck.lock().expect("contention health lock poisoned").remove(&id);
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            waits: self.waits.load(Ordering::Relaxed),
            long_waits: self.long_waits.load(Ordering::Relaxed),
            longest_ms: self.longest_ms.load(Ordering::Relaxed),
            stuck: self
                .stuck
                .lock()
                .expect("contention health lock poisoned")
                .values()
                .cloned()
                .collect(),
        }
    }
}

/// Registry for waits on connections that have no guard, and so no
/// `WorkDb`, to report to.
pub(crate) fn fallback_health() -> Arc<Health> {
    static HEALTH: std::sync::OnceLock<Arc<Health>> = std::sync::OnceLock::new();
    HEALTH.get_or_init(Arc::default).clone()
}

#[cfg(test)]
#[path = "contention_tests.rs"]
mod tests;
