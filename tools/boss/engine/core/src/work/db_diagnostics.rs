//! SQLite call observations, task attribution and scoped maintenance deadlines.
use std::{
    cell::RefCell,
    collections::HashMap,
    future::Future,
    panic::Location,
    rc::Rc,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default)]
pub(crate) struct Attribution {
    pub verb: String,
    pub request_id: String,
    pub session_id: String,
    pub background: String,
}

tokio::task_local! { static CONTEXT: Attribution; }

impl Attribution {
    pub(crate) async fn scope<F: Future>(self, future: F) -> F::Output {
        CONTEXT.scope(self, future).await
    }
}

/// Preserve RPC attribution in child tasks; otherwise name the spawning loop.
#[track_caller]
pub(crate) fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let caller = Location::caller();
    let attribution = CONTEXT.try_with(Clone::clone).unwrap_or_else(|_| Attribution {
        background: format!("{} @ {caller}", std::any::type_name::<F>()),
        ..Attribution::default()
    });
    boss_task_scope::spawn(attribution.scope(future))
}

/// Task-local values do not cross Tokio's blocking boundary automatically.
#[track_caller]
pub(crate) fn spawn_blocking<F, R>(work: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let caller = Location::caller();
    let attribution = CONTEXT.try_with(Clone::clone).unwrap_or_else(|_| Attribution {
        background: format!("{} @ {caller}", std::any::type_name::<F>()),
        ..Attribution::default()
    });
    tokio::task::spawn_blocking(move || CONTEXT.sync_scope(attribution, work))
}

crate::register_counter!(
    DATABASE_BUSY,
    "work_database_busy",
    "SQLite BUSY statement failures, including rate-limited holder diagnostics."
);

pub(crate) fn register_metrics(registry: &crate::metrics::Registry) {
    registry.register_counter(&DATABASE_BUSY);
}

#[derive(bon::Builder)]
#[builder(on(String, into))]
pub(super) struct Observation {
    pub caller: &'static Location<'static>,
    pub attribution: Attribution,
    database: String,
    metrics: std::sync::Arc<crate::metrics::Registry>,
    query_time: std::cell::Cell<Duration>,
    writing: std::cell::Cell<bool>,
}

thread_local! {
    static QUERY_DEADLINE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    // A connection guard cannot migrate between threads. Avoid a global lock
    // for every returned row; only writer transitions and BUSY touch the registry.
    static ACTIVE: RefCell<HashMap<usize, Rc<Observation>>> = RefCell::default();
}

/// Cooperatively interrupt maintenance SQL without replacing the connection's
/// observation hook. Drop before releasing the connection, including on error.
pub(super) struct QueryDeadline(Option<Instant>);

impl QueryDeadline {
    pub(super) fn after(duration: Duration) -> Self {
        Self(QUERY_DEADLINE.replace(Some(Instant::now() + duration)))
    }
}

impl Drop for QueryDeadline {
    fn drop(&mut self) {
        QUERY_DEADLINE.set(self.0);
    }
}

#[derive(Clone, Debug)]
struct Holder {
    caller: &'static Location<'static>,
    attribution: Attribution,
    database: String,
    since: Instant,
}

#[derive(Default)]
struct BusyCount {
    total: u64,
    suppressed: u64,
    last_dump: Option<Instant>,
}

static HOLDERS: LazyLock<Mutex<HashMap<usize, Holder>>> = LazyLock::new(Mutex::default);
static BUSY: LazyLock<Mutex<HashMap<&'static Location<'static>, BusyCount>>> = LazyLock::new(Mutex::default);

pub(super) fn start(
    conn: &rusqlite::Connection,
    caller: &'static Location<'static>,
    metrics: std::sync::Arc<crate::metrics::Registry>,
) -> (usize, Rc<Observation>) {
    rusqlite::trace::set_call_observer(observe);
    // SAFETY: identity only; never dereferenced or used across threads.
    let id = unsafe { conn.handle() } as usize;
    let observation = Rc::new(
        Observation::builder()
            .caller(caller)
            .attribution(CONTEXT.try_with(Clone::clone).unwrap_or_else(|_| Attribution {
                background: caller.file().to_owned(),
                ..Attribution::default()
            }))
            .database(conn.path().unwrap_or(":memory:"))
            .metrics(metrics)
            .query_time(std::cell::Cell::new(Duration::ZERO))
            .writing(std::cell::Cell::new(false))
            .build(),
    );
    ACTIVE.with_borrow_mut(|active| active.insert(id, observation.clone()));
    (id, observation)
}

/// Independent WorkDb connections (notably backups) need the same observations.
pub(crate) struct OwnedConnection {
    connection: Option<rusqlite::Connection>,
    id: usize,
    observation: Rc<Observation>,
    acquired: Instant,
}

impl OwnedConnection {
    #[track_caller]
    pub(super) fn new(connection: rusqlite::Connection, metrics: std::sync::Arc<crate::metrics::Registry>) -> Self {
        let (id, observation) = start(&connection, Location::caller(), metrics);
        Self {
            connection: Some(connection),
            id,
            observation,
            acquired: Instant::now(),
        }
    }
}

impl std::ops::Deref for OwnedConnection {
    type Target = rusqlite::Connection;
    fn deref(&self) -> &Self::Target {
        self.connection.as_ref().expect("live independent connection")
    }
}

impl std::ops::DerefMut for OwnedConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection.as_mut().expect("live independent connection")
    }
}

impl Drop for OwnedConnection {
    fn drop(&mut self) {
        drop(self.connection.take());
        finish(self.id);
        let held = self.acquired.elapsed();
        if held >= Duration::from_millis(100) {
            tracing::warn!(wait_ms = 0, held_ms = held.as_millis() as u64,
                query_ms = self.observation.query_time().as_secs_f64() * 1000.0,
                caller = %self.observation.caller, attribution = ?self.observation.attribution,
                "work DB connection lock timing");
        }
    }
}

pub(super) fn finish(id: usize) {
    ACTIVE.with_borrow_mut(|active| active.remove(&id));
    HOLDERS.lock().unwrap().remove(&id);
}

impl Observation {
    pub(super) fn query_time(&self) -> Duration {
        self.query_time.get()
    }

    fn set_writing(&self, id: usize, writing: bool) {
        if self.writing.replace(writing) == writing {
            return;
        }
        let mut holders = HOLDERS.lock().unwrap();
        if writing {
            holders.insert(
                id,
                Holder {
                    caller: self.caller,
                    attribution: self.attribution.clone(),
                    database: self.database.clone(),
                    since: Instant::now(),
                },
            );
        } else {
            holders.remove(&id);
        }
    }
}

/// Called once a write actually changes a row, including autocommit writes
/// that may still be executing when another connection reports BUSY.
pub(super) fn install_update_observer(conn: &rusqlite::Connection) {
    // SAFETY: callbacks run synchronously on this connection's thread. The
    // progress callback only inspects transaction state; it never executes SQL.
    let id = unsafe { conn.handle() } as usize;
    conn.update_hook(Some(move |_: rusqlite::hooks::Action, _: &str, _: &str, _: i64| {
        ACTIVE.with_borrow(|active| {
            if let Some(observation) = active.get(&id) {
                observation.set_writing(id, true);
            }
        });
    }));
    // A no-op UPDATE can hold the writer while scanning many rows without
    // firing an update hook. Observe that ownership during execution, too.
    conn.progress_handler(
        1000,
        Some(move || {
            let writing = unsafe {
                rusqlite::ffi::sqlite3_txn_state(id as *mut rusqlite::ffi::sqlite3, c"main".as_ptr())
                    == rusqlite::ffi::SQLITE_TXN_WRITE
            };
            ACTIVE.with_borrow(|active| {
                if let Some(observation) = active.get(&id) {
                    observation.set_writing(id, writing);
                }
            });
            QUERY_DEADLINE.get().is_some_and(|deadline| Instant::now() >= deadline)
        }),
    );
}

fn observe(call: rusqlite::trace::CallObservation) {
    ACTIVE.with_borrow(|active| {
        let Some(observation) = active.get(&call.connection) else {
            return;
        };
        observation.query_time.set(observation.query_time.get() + call.elapsed);
        observation.set_writing(
            call.connection,
            call.transaction_state == rusqlite::ffi::SQLITE_TXN_WRITE,
        );
        if call.result & 0xff == rusqlite::ffi::SQLITE_BUSY {
            report_busy(observation, call.result);
        }
    });
}

fn report_busy(observation: &Observation, result: i32) {
    DATABASE_BUSY.inc(&observation.metrics);
    let (total, suppressed) = {
        let mut counts = BUSY.lock().unwrap();
        let count = counts.entry(observation.caller).or_default();
        count.total += 1;
        if count
            .last_dump
            .is_some_and(|last| last.elapsed() < Duration::from_secs(1))
        {
            count.suppressed += 1;
            return;
        }
        count.last_dump = Some(Instant::now());
        (count.total, std::mem::take(&mut count.suppressed))
    };
    let holders: Vec<_> = HOLDERS
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, holder)| holder.database == observation.database)
        .map(|(id, holder)| {
            format!(
                "connection={id:x} caller={} attribution={:?} held_ms={}",
                holder.caller,
                holder.attribution,
                holder.since.elapsed().as_millis()
            )
        })
        .collect();
    tracing::warn!(caller = %observation.caller, attribution = ?observation.attribution,
        sqlite_result = result, busy_count = total, suppressed_dumps = suppressed, ?holders,
        "work DB SQLITE_BUSY holder diagnostic");
}

/// Included in read-admission dumps so the named RPC stage can be connected
/// to a SQLite writer, including a background sweep outside the read lanes.
pub(crate) fn holder_snapshot() -> Vec<String> {
    HOLDERS
        .lock()
        .unwrap()
        .iter()
        .map(|(id, holder)| {
            format!(
                "connection={id:x} caller={} attribution={:?} held_ms={}",
                holder.caller,
                holder.attribution,
                holder.since.elapsed().as_millis()
            )
        })
        .collect()
}

#[cfg(test)]
mod tests;
