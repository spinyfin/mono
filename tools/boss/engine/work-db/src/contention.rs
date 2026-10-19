//! Transient SQLite lock contention, told apart from real database failures
//! in one place, and absorbed instead of returned to clients.
//!
//! SQLite reports a lock it could not take as `SQLITE_BUSY` (another
//! connection, possibly in another process, holds the lock) or
//! `SQLITE_LOCKED` (a conflict inside one process's shared cache). Neither is
//! something a client can act on: the right response is to wait and try
//! again, so the engine's connections never surface them as "database is
//! locked" to the macOS app or the CLI.
//!
//! The busy handler installed by [`install`] retries the lock acquisition
//! with SQLite's own backoff schedule and does not give up: a wait that never
//! clears is reported through the engine health surface ([`Health::snapshot`]) as an
//! engine problem, not through the failing operation. Because the handler
//! runs *inside* the statement that could not take the lock, the enclosing
//! transaction is never rolled back and retried from outside — for a
//! `BEGIN IMMEDIATE` or autocommit statement the lock is taken before any row
//! is touched, so a retried write is applied exactly once.
//!
//! The handler is not invoked for `SQLITE_LOCKED` or `SQLITE_BUSY_SNAPSHOT`.
//! Neither can occur on a production connection: those databases are files,
//! opened without shared cache (so there is no `SQLITE_LOCKED`), and every
//! write transaction begins `IMMEDIATE` (so a transaction never upgrades from
//! a stale read snapshot). `contention_tests` pins both invariants.
//!
//! Connections opened for CLI inspection (`bossctl`) do not use the
//! absorbing handler; they wait a bounded time and then fail with SQLite's
//! own error, so an operator tool never hangs on a stuck lock.
//!
//! Every wait is logged with the caller that holds the connection, the
//! frontend request kind being served (when known), the attempt count and
//! the total time waited, so contention stays visible to operators even
//! though it no longer surfaces to users.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// How a connection waits for a lock another connection holds.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LockWait {
    /// Retry until the lock is free (see [`absorb`]). Every connection the
    /// engine opens uses this.
    Absorb,
    /// Fail with SQLite's own error after this long. For CLI inspection
    /// connections, which must not hang on a stuck lock.
    Bounded(Duration),
}

impl LockWait {
    pub(crate) fn apply(self, conn: &Connection) -> rusqlite::Result<()> {
        match self {
            Self::Absorb => conn.busy_handler(Some(absorb)),
            Self::Bounded(limit) => conn.busy_timeout(limit),
        }
    }
}

/// A wait longer than this is an engine-health problem. The connection layer
/// logs any lock hold of 100 ms or more (`connection_wait`), and engine write
/// transactions are expected to commit well inside that, so a wait of five
/// seconds is fifty times the point at which a hold is already considered
/// slow: another holder is stuck, not the engine busy. It is also the bound
/// the CLI's direct-DB connections wait before failing.
///
/// Measured on a fresh file-backed database: 200 `create_chore`
/// transactions took p50 1.1 ms, p90 4.5 ms, p99 436 ms, max 751 ms; 200
/// `set_metadata` autocommit writes took p50 18 µs, p99 189 µs, max 1.0 ms.
/// The worst engine transaction is under a second.
pub(crate) const HEALTH_THRESHOLD: Duration = Duration::from_secs(5);

/// First progress line for a wait still in flight, then every
/// `PROGRESS_INTERVAL` after it, so a wait that never clears keeps logging.
const PROGRESS_FIRST: Duration = Duration::from_secs(1);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// SQLite's own `sqliteDefaultBusyCallback` schedule (ms), then a steady
/// 100 ms: short enough that a lock released in microseconds is retried
/// promptly, long enough that a stuck lock costs no CPU.
const BACKOFF_MS: [u64; 12] = [1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100];

/// A stuck wait whose last attempt is older than this is treated as cleared:
/// the longest backoff step plus slack for a descheduled thread. A wait that
/// is genuinely still stuck retries at least every 100 ms.
const STALE_AFTER: Duration = Duration::from_secs(2);

tokio::task_local! {
    /// The frontend request kind being served by the current task, set by
    /// the dispatch loop so the DB layer can name it in contention logs.
    pub(crate) static REQUEST_KIND: Arc<str>;
    /// Whether the read being served is blocked in the busy handler, set for
    /// reads that coalesce (see `app::read_coalesce`).
    pub(crate) static READ_WAIT: Arc<ReadWait>;
}

/// Request kind for the current task, or `"internal"` outside a frontend
/// dispatch (sweeps, pollers, and handlers that moved their DB call to a
/// separate blocking task).
pub(crate) fn current_request_kind() -> Arc<str> {
    REQUEST_KIND
        .try_with(|kind| kind.clone())
        .unwrap_or_else(|_| Arc::from("internal"))
}

/// Set while a read's database call is waiting for a SQLite lock. A read
/// that is still waiting has not taken its snapshot yet, so an identical read
/// that arrives meanwhile can be answered from it without being answered
/// from older data.
#[derive(Debug, Default)]
pub(crate) struct ReadWait(AtomicBool);

impl ReadWait {
    pub(crate) fn is_waiting(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn set(&self, waiting: bool) {
        self.0.store(waiting, Ordering::Release);
    }
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

impl Holder {
    /// The calling code as holder, for the current request, reporting to
    /// `health`.
    #[track_caller]
    pub(crate) fn here(health: &Arc<Health>) -> Self {
        Self {
            caller: std::panic::Location::caller(),
            request: current_request_kind(),
            health: health.clone(),
        }
    }
}

/// One lock wait in flight on this thread.
#[derive(bon::Builder)]
struct Wait {
    started: Instant,
    /// When the most recent backoff sleep ended. The lock is taken on the
    /// attempt that follows it, so this is the wait's end as far as the
    /// handler can tell.
    last_attempt: Instant,
    #[builder(default)]
    attempts: u32,
    next_progress: Duration,
    holder: Holder,
    /// Callers of the other connection guards this thread held when the wait
    /// began. Non-empty means the wait may be against the thread itself.
    thread_holders: Vec<String>,
    read_wait: Option<Arc<ReadWait>>,
    /// Set once the wait is past [`HEALTH_THRESHOLD`] and registered as
    /// stuck, to the id it is registered under.
    reported: Option<u64>,
}

impl Wait {
    fn waited(&self) -> Duration {
        self.last_attempt.saturating_duration_since(self.started)
    }
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
        // A wait still recorded here ended before this scope began.
        finish_wait();
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

/// The innermost guard on this thread, or — for a connection used without
/// one (tests, which open independent connections directly) — an anonymous
/// holder reporting to the process-wide fallback registry.
fn current_holder() -> Holder {
    HOLDERS
        .with(|holders| holders.borrow().last().cloned())
        .unwrap_or_else(|| Holder {
            caller: std::panic::Location::caller(),
            request: Arc::from("unknown"),
            health: fallback_health(),
        })
}

/// Callers of every guard on this thread except the innermost.
fn outer_holders() -> Vec<String> {
    HOLDERS.with(|holders| {
        let holders = holders.borrow();
        holders
            .iter()
            .take(holders.len().saturating_sub(1))
            .map(|holder| holder.caller.to_string())
            .collect()
    })
}

fn begin_wait() {
    let holder = current_holder();
    let read_wait = READ_WAIT.try_with(Arc::clone).ok();
    if let Some(read_wait) = &read_wait {
        read_wait.set(true);
    }
    let now = Instant::now();
    WAIT.with(|wait| {
        *wait.borrow_mut() = Some(
            Wait::builder()
                .started(now)
                .last_attempt(now)
                .next_progress(PROGRESS_FIRST)
                .holder(holder)
                .thread_holders(outer_holders())
                .maybe_read_wait(read_wait)
                .build(),
        )
    });
}

/// The busy handler. `attempt` is SQLite's count of retries for the
/// current lock acquisition, starting at 0; returning `true` asks SQLite to
/// try again. A new acquisition (attempt 0) ends any previous wait on this
/// thread, which is the only signal SQLite gives that a wait cleared short
/// of the guard being dropped.
///
/// It returns `false` only for a wait that has outlasted [`HEALTH_THRESHOLD`]
/// on a thread that already holds another engine connection: if that other
/// connection holds the write lock, nothing can ever release it, and failing
/// loudly beats hanging the thread (and the writer mutex) forever.
fn absorb(attempt: i32) -> bool {
    if attempt == 0 {
        finish_wait();
        begin_wait();
    }
    let step = WAIT.with(|wait| {
        let mut wait = wait.borrow_mut();
        let Some(wait) = wait.as_mut() else {
            return Step::Retry(Duration::from_millis(100));
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
            if !wait.thread_holders.is_empty() {
                return Step::GiveUp;
            }
            let id = *wait
                .reported
                .get_or_insert_with(|| NEXT_WAIT_ID.fetch_add(1, Ordering::Relaxed));
            wait.holder.health.stuck(id, &wait.holder, wait.started, wait.attempts);
        }
        Step::Retry(Duration::from_millis(
            BACKOFF_MS
                .get(usize::try_from(attempt).unwrap_or(usize::MAX).min(BACKOFF_MS.len()))
                .copied()
                .unwrap_or(100),
        ))
    });
    match step {
        Step::Retry(delay) => {
            sleep_yielding(delay);
            WAIT.with(|wait| {
                if let Some(wait) = wait.borrow_mut().as_mut() {
                    wait.last_attempt = Instant::now();
                }
            });
            true
        }
        Step::GiveUp => {
            give_up();
            false
        }
    }
}

enum Step {
    Retry(Duration),
    GiveUp,
}

/// Abandon the wait in flight on this thread, logging why.
fn give_up() {
    let Some(wait) = WAIT.with(|wait| wait.borrow_mut().take()) else {
        return;
    };
    if let Some(read_wait) = &wait.read_wait {
        read_wait.set(false);
    }
    tracing::error!(
        caller = %wait.holder.caller,
        request = %wait.holder.request,
        attempts = wait.attempts,
        waited_ms = wait.started.elapsed().as_millis() as u64,
        thread_holders = ?wait.thread_holders,
        "work DB contention: this thread already holds another engine connection and has waited past the \
         health threshold for the SQLite lock; it is probably waiting for itself, so the statement fails"
    );
}

/// End the wait in flight on this thread, logging its summary.
fn finish_wait() {
    let Some(wait) = WAIT.with(|wait| wait.borrow_mut().take()) else {
        return;
    };
    if let Some(read_wait) = &wait.read_wait {
        read_wait.set(false);
    }
    let waited = wait.waited();
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
/// connections `query_connection` and `reader` hand out). Counters cover the
/// handle's lifetime; `stuck` holds only waits currently past
/// [`HEALTH_THRESHOLD`] that are still retrying.
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
    last_attempt: Instant,
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
            last_attempt: Instant::now(),
        });
        entry.attempts = attempts;
        entry.last_attempt = Instant::now();
    }

    fn cleared(&self, id: u64) {
        self.stuck.lock().expect("contention health lock poisoned").remove(&id);
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        let mut stuck = self.stuck.lock().expect("contention health lock poisoned");
        // A wait that stopped retrying has ended even if nothing has
        // recorded that yet (its thread has not released its guard).
        stuck.retain(|_, wait| wait.last_attempt.elapsed() <= STALE_AFTER);
        Snapshot {
            waits: self.waits.load(Ordering::Relaxed),
            long_waits: self.long_waits.load(Ordering::Relaxed),
            longest_ms: self.longest_ms.load(Ordering::Relaxed),
            stuck: stuck.values().cloned().collect(),
        }
    }
}

/// Registry for waits on connections used without a guard or scope, and so
/// no `WorkDb`, to report to.
pub(crate) fn fallback_health() -> Arc<Health> {
    static HEALTH: std::sync::OnceLock<Arc<Health>> = std::sync::OnceLock::new();
    HEALTH.get_or_init(Arc::default).clone()
}

#[cfg(test)]
#[path = "contention_tests.rs"]
mod tests;
