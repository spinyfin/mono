//! Page-read accounting for work DB connections; observes only.
//!
//! SQLite's pager counts every page a connection fetches from the database
//! file or its WAL because the page was not in that connection's cache. That
//! count is what a query costs the process in reads: a walk of a table larger
//! than the cache is one `pread` per page, every time it runs. This module
//! attributes the count to the connection hold that caused it and, inside a
//! hold, to statement text, so a statement that walks a large table (once, or
//! once per row of an outer loop) is named in the engine log with its caller.
//!
//! Three surfaces, all fed from the same numbers:
//!
//! - `work DB heavy read`: one WARN per hold that reads at least
//!   [`heavy_hold_pages`] pages, rate-limited per caller, naming the caller,
//!   its attribution and the statement that read the most.
//! - `db read summary`: one INFO line per minute that saw reads worth naming,
//!   listing the callers that read the most and how many holds each took.
//! - `db_read.*` metrics.
//!
//! Nothing here changes a cache size, a query plan or a lock.
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, c_int, c_uint, c_void};
use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use rusqlite::ffi;

/// Pages a single hold may read before it is reported: 64 MiB at the
/// engine's 4 KiB page size. Far above any indexed read, far below one pass
/// over a table that has outgrown the cache.
const DEFAULT_HEAVY_HOLD_PAGES: u64 = 16_384;
/// Overrides [`DEFAULT_HEAVY_HOLD_PAGES`], for example to list every reader
/// of an isolated engine during an investigation.
pub const HEAVY_HOLD_PAGES_ENV: &str = "BOSS_WORK_DB_HEAVY_READ_PAGES";
/// Statement executions that read fewer pages are not tracked by text.
const STATEMENT_NOTE_PAGES: u64 = 64;
/// Holds that read fewer pages stay out of the per-minute caller rollup.
const ROLLUP_NOTE_PAGES: u64 = 256;
/// Distinct statement texts tracked per hold; the rest are summed as one.
const MAX_NOTED_STATEMENTS: usize = 32;
const OTHER_STATEMENTS: &str = "(other statements)";
const TOP_CALLERS: usize = 5;
const SQL_PREVIEW_CHARS: usize = 240;
const WARN_INTERVAL: Duration = Duration::from_secs(10);

boss_metrics::register_counter!(
    DB_READ_PAGES,
    "db_read.pages",
    "Pages work DB connections fetched from the database file or WAL (pager cache misses)."
);
boss_metrics::register_counter!(
    DB_READ_HEAVY_HOLDS,
    "db_read.heavy_holds",
    "Work DB connection holds that read at least the heavy-read page threshold."
);
boss_metrics::register_gauge!(
    DB_READ_HEAVIEST_HOLD_PAGES,
    "db_read.heaviest_hold_pages",
    "Most pages read by one work DB connection hold in the last summary interval (0 when none)."
);

pub fn register_metrics(registry: &boss_metrics::Registry) {
    registry.register_counter(&DB_READ_PAGES);
    registry.register_counter(&DB_READ_HEAVY_HOLDS);
    registry.register_gauge(&DB_READ_HEAVIEST_HOLD_PAGES);
}

pub fn heavy_hold_pages() -> u64 {
    static PAGES: LazyLock<u64> =
        LazyLock::new(|| boss_engine_utils::env_parse::env_parsed_or(HEAVY_HOLD_PAGES_ENV, DEFAULT_HEAVY_HOLD_PAGES));
    (*PAGES).max(1)
}

/// The pager's cache-miss count for every database attached to `db`. A
/// 32-bit counter in SQLite, so callers subtract with wrapping arithmetic.
fn cache_misses(db: *mut ffi::sqlite3) -> u32 {
    let (mut current, mut high): (c_int, c_int) = (0, 0);
    // SAFETY: a status read on a live connection owned by the calling thread.
    unsafe { ffi::sqlite3_db_status(db, ffi::SQLITE_DBSTATUS_CACHE_MISS, &mut current, &mut high, 0) };
    current as u32
}

/// One statement text's share of a hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatementReads {
    /// SQL text with whitespace collapsed, truncated. Never bound values.
    pub sql: String,
    pub runs: u64,
    pub pages: u64,
    /// Rows stepped by a full table scan, as SQLite counts them.
    pub full_scan_steps: u64,
    pub elapsed: Duration,
}

/// What one connection hold read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HoldReads {
    pub pages: u64,
    pub statements: u64,
    pub full_scan_steps: u64,
    /// The statement text that read the most pages, if any crossed the
    /// tracking floor.
    pub top: Option<StatementReads>,
}

impl StatementReads {
    fn named(sql: String) -> Self {
        Self {
            sql,
            runs: 0,
            pages: 0,
            full_scan_steps: 0,
            elapsed: Duration::ZERO,
        }
    }
}

/// Per-hold accumulator, owned by the hold's `Observation` and touched only
/// on the connection's thread.
#[derive(Default)]
pub(crate) struct Tracker {
    mark: Cell<u32>,
    pages: Cell<u64>,
    statements: Cell<u64>,
    full_scan_steps: Cell<u64>,
    noted: RefCell<Vec<StatementReads>>,
}

impl Tracker {
    /// Start counting from the connection's current position, so reads made
    /// before this hold (by an earlier hold, or by connection setup) are not
    /// charged to it.
    pub(super) fn start(conn: &rusqlite::Connection) -> Self {
        let tracker = Self::default();
        // SAFETY: the handle is live for the borrow of `conn`.
        tracker.mark.set(cache_misses(unsafe { conn.handle() }));
        tracker
    }

    fn advance(&self, db: *mut ffi::sqlite3) -> u64 {
        let now = cache_misses(db);
        let pages = u64::from(now.wrapping_sub(self.mark.replace(now)));
        self.pages.set(self.pages.get() + pages);
        pages
    }

    fn statement_finished(&self, db: *mut ffi::sqlite3, stmt: *mut ffi::sqlite3_stmt, elapsed: Duration) {
        let pages = self.advance(db);
        self.statements.set(self.statements.get() + 1);
        // SAFETY: `stmt` is the live statement SQLite is reporting on. Reading
        // with reset keeps a cached statement from being counted twice.
        let scans = unsafe { ffi::sqlite3_stmt_status(stmt, ffi::SQLITE_STMTSTATUS_FULLSCAN_STEP, 1) };
        let scans = u64::try_from(scans).unwrap_or(0);
        self.full_scan_steps.set(self.full_scan_steps.get() + scans);
        if pages < STATEMENT_NOTE_PAGES {
            return;
        }
        // SAFETY: the text belongs to `stmt` and outlives this call.
        let text = unsafe { ffi::sqlite3_sql(stmt) };
        let sql = if text.is_null() {
            String::new()
        } else {
            // SAFETY: SQLite returns a NUL-terminated string.
            preview(&unsafe { CStr::from_ptr(text) }.to_string_lossy())
        };
        // Keyed by text, not by statement: an uncached statement prepared
        // once per row of an outer loop is one entry with many runs.
        let mut noted = self.noted.borrow_mut();
        let index = match noted.iter().position(|known| known.sql == sql) {
            Some(index) => index,
            None if noted.len() < MAX_NOTED_STATEMENTS => {
                noted.push(StatementReads::named(sql));
                noted.len() - 1
            }
            None => match noted.iter().position(|known| known.sql == OTHER_STATEMENTS) {
                Some(index) => index,
                None => {
                    noted.push(StatementReads::named(OTHER_STATEMENTS.to_owned()));
                    noted.len() - 1
                }
            },
        };
        let entry = &mut noted[index];
        entry.runs += 1;
        entry.pages += pages;
        entry.full_scan_steps += scans;
        entry.elapsed += elapsed;
    }

    /// Close the hold: charge it any reads made since the last statement
    /// finished and return its totals.
    pub(super) fn finish(&self, conn: &rusqlite::Connection) -> HoldReads {
        // SAFETY: the handle is live for the borrow of `conn`.
        self.advance(unsafe { conn.handle() });
        HoldReads {
            pages: self.pages.get(),
            statements: self.statements.get(),
            full_scan_steps: self.full_scan_steps.get(),
            top: self.noted.take().into_iter().max_by_key(|reads| reads.pages),
        }
    }
}

fn preview(sql: &str) -> String {
    let mut out = String::with_capacity(SQL_PREVIEW_CHARS.min(sql.len()));
    for word in sql.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() >= SQL_PREVIEW_CHARS {
            let cut: String = out.chars().take(SQL_PREVIEW_CHARS).collect();
            return format!("{cut}...");
        }
    }
    out
}

unsafe extern "C" fn on_trace(event: c_uint, _: *mut c_void, subject: *mut c_void, detail: *mut c_void) -> c_int {
    if event == ffi::SQLITE_TRACE_PROFILE as c_uint {
        // SAFETY: for a profile event SQLite passes the finished statement
        // and a pointer to its run time in nanoseconds.
        unsafe { statement_finished(subject.cast(), *detail.cast::<i64>()) };
    }
    0
}

/// Charge the statement SQLite just finished to the hold that ran it.
///
/// # Safety
///
/// `stmt` must be a live statement on a connection owned by this thread.
pub(super) unsafe fn statement_finished(stmt: *mut ffi::sqlite3_stmt, nanos: i64) {
    // SAFETY: guaranteed by the caller.
    let db = unsafe { ffi::sqlite3_db_handle(stmt) };
    super::ACTIVE.with_borrow(|active| {
        if let Some(observation) = active.get(&(db as usize)) {
            let elapsed = Duration::from_nanos(u64::try_from(nanos).unwrap_or(0));
            observation.reads.statement_finished(db, stmt, elapsed);
        }
    });
}

/// Report every finished statement on `conn` to its hold. A connection has
/// one trace callback: [`super::StatementCounter`] shares it for its lifetime
/// and puts this one back.
pub(super) fn install(conn: &rusqlite::Connection) {
    // SAFETY: the callback takes no context and only reads status counters.
    unsafe {
        ffi::sqlite3_trace_v2(
            conn.handle(),
            ffi::SQLITE_TRACE_PROFILE as c_uint,
            Some(on_trace),
            std::ptr::null_mut(),
        );
    }
}

#[derive(Default)]
struct CallerWindow {
    holds: u64,
    pages: u64,
    heaviest: u64,
    attribution: String,
    top: Option<StatementReads>,
}

#[derive(Default)]
struct WarnState {
    last: Option<Instant>,
    suppressed_holds: u64,
    suppressed_pages: u64,
}

static TOTAL_PAGES: AtomicU64 = AtomicU64::new(0);
static HEAVY_HOLDS: AtomicU64 = AtomicU64::new(0);
static WINDOW: LazyLock<Mutex<HashMap<&'static Location<'static>, CallerWindow>>> = LazyLock::new(Mutex::default);
static WARNED: LazyLock<Mutex<HashMap<&'static Location<'static>, WarnState>>> = LazyLock::new(Mutex::default);

thread_local! {
    // Holds released on this thread while a `measure` call is running.
    static MEASURING: RefCell<Option<Measured>> = const { RefCell::new(None) };
}

/// What the connection holds released during one [`measure`] call read.
#[derive(Clone, Debug, Default)]
pub struct Measured {
    pub pages: u64,
    pub statements: u64,
    pub full_scan_steps: u64,
    /// Each hold with the location that took it, in release order.
    pub holds: Vec<(String, HoldReads)>,
}

impl Measured {
    /// The statement text that read the most pages in any one hold.
    pub fn top_statement(&self) -> Option<&StatementReads> {
        self.holds
            .iter()
            .filter_map(|(_, reads)| reads.top.as_ref())
            .max_by_key(|top| top.pages)
    }
}

/// Run `work` and report what the work DB connections it took and released
/// on this thread read. For asserting a read budget on one code path: reads
/// made on other threads, or by a hold still open when `work` returns, are
/// not included. Calls do not nest.
pub fn measure<T>(work: impl FnOnce() -> T) -> (T, Measured) {
    MEASURING.with_borrow_mut(|slot| *slot = Some(Measured::default()));
    let result = work();
    let measured = MEASURING.with_borrow_mut(Option::take).unwrap_or_default();
    (result, measured)
}

fn label(attribution: &super::Attribution) -> String {
    if attribution.verb.is_empty() {
        attribution.background.clone()
    } else {
        format!("rpc {}", attribution.verb)
    }
}

/// Whether a hold is expected to read the whole file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// A read that should cost what it returns.
    Query,
    /// A copy of the whole database, such as a backup snapshot. Counted and
    /// summarized like any other hold, but not warned about: it reads every
    /// page by design, and a warning that fires on schedule teaches people
    /// to ignore the line.
    WholeFile,
}

fn warrants_warning(pages: u64, scope: Scope) -> bool {
    scope == Scope::Query && pages >= heavy_hold_pages()
}

/// Record a finished hold. Call after the connection has been released: this
/// takes its own small locks and may log.
pub(crate) fn record(
    caller: &'static Location<'static>,
    attribution: &super::Attribution,
    held: Duration,
    reads: &HoldReads,
    scope: Scope,
) {
    MEASURING.with_borrow_mut(|slot| {
        if let Some(measured) = slot {
            measured.pages += reads.pages;
            measured.statements += reads.statements;
            measured.full_scan_steps += reads.full_scan_steps;
            measured.holds.push((caller.to_string(), reads.clone()));
        }
    });
    if reads.pages == 0 {
        return;
    }
    TOTAL_PAGES.fetch_add(reads.pages, Ordering::Relaxed);
    if reads.pages < ROLLUP_NOTE_PAGES.min(heavy_hold_pages()) {
        return;
    }
    {
        let mut window = WINDOW.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = window.entry(caller).or_default();
        entry.holds += 1;
        entry.pages += reads.pages;
        if reads.pages >= entry.heaviest {
            entry.heaviest = reads.pages;
            entry.attribution = label(attribution);
            entry.top.clone_from(&reads.top);
        }
    }
    if !warrants_warning(reads.pages, scope) {
        return;
    }
    HEAVY_HOLDS.fetch_add(1, Ordering::Relaxed);
    let (suppressed_holds, suppressed_pages) = {
        let mut warned = WARNED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = warned.entry(caller).or_default();
        if state.last.is_some_and(|last| last.elapsed() < WARN_INTERVAL) {
            state.suppressed_holds += 1;
            state.suppressed_pages += reads.pages;
            return;
        }
        state.last = Some(Instant::now());
        (
            std::mem::take(&mut state.suppressed_holds),
            std::mem::take(&mut state.suppressed_pages),
        )
    };
    let top = reads
        .top
        .clone()
        .unwrap_or_else(|| StatementReads::named(String::new()));
    tracing::warn!(
        caller = %caller,
        verb = %attribution.verb,
        request_id = %attribution.request_id,
        background = %attribution.background,
        pages_read = reads.pages,
        statements = reads.statements,
        full_scan_steps = reads.full_scan_steps,
        held_ms = held.as_millis() as u64,
        top_statement_pages = top.pages,
        top_statement_runs = top.runs,
        top_statement_full_scan_steps = top.full_scan_steps,
        top_statement_ms = top.elapsed.as_millis() as u64,
        top_statement = %top.sql,
        suppressed_holds,
        suppressed_pages,
        "work DB heavy read"
    );
}

/// One caller's reads over a summary interval.
#[derive(Debug, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct CallerReads {
    pub caller: String,
    pub attribution: String,
    pub holds: u64,
    pub pages: u64,
    pub heaviest_hold_pages: u64,
    pub top_statement: Option<StatementReads>,
}

/// Drain the interval: total pages, heavy holds, and callers by pages read.
pub fn take_summary() -> (u64, u64, Vec<CallerReads>) {
    let window = std::mem::take(&mut *WINDOW.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    let mut callers: Vec<_> = window
        .into_iter()
        .map(|(caller, entry)| {
            CallerReads::builder()
                .caller(caller.to_string())
                .attribution(entry.attribution)
                .holds(entry.holds)
                .pages(entry.pages)
                .heaviest_hold_pages(entry.heaviest)
                .maybe_top_statement(entry.top)
                .build()
        })
        .collect();
    callers.sort_by(|a, b| b.pages.cmp(&a.pages).then_with(|| a.caller.cmp(&b.caller)));
    (
        TOTAL_PAGES.swap(0, Ordering::Relaxed),
        HEAVY_HOLDS.swap(0, Ordering::Relaxed),
        callers,
    )
}

/// Publish the interval to `registry` and log one line if any caller read
/// enough to be named. `interval` is only reported, never used to schedule.
pub fn flush(registry: &boss_metrics::Registry, interval: Duration) {
    let (pages, heavy_holds, callers) = take_summary();
    DB_READ_PAGES.inc_by(registry, pages);
    DB_READ_HEAVY_HOLDS.inc_by(registry, heavy_holds);
    let heaviest = callers.iter().map(|c| c.heaviest_hold_pages).max().unwrap_or(0);
    DB_READ_HEAVIEST_HOLD_PAGES.set(registry, i64::try_from(heaviest).unwrap_or(i64::MAX));
    if callers.is_empty() {
        return;
    }
    let top = callers
        .iter()
        .take(TOP_CALLERS)
        .map(|c| {
            let statement = c.top_statement.as_ref().map_or_else(String::new, |s| {
                format!(" top_statement_pages={} top_statement=[{}]", s.pages, s.sql)
            });
            format!(
                "{} [{}] holds={} pages={} heaviest_hold_pages={}{statement}",
                c.caller, c.attribution, c.holds, c.pages, c.heaviest_hold_pages
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    tracing::info!(
        interval_secs = interval.as_secs(),
        pages_read = pages,
        heavy_holds,
        callers = callers.len(),
        top_callers = %top,
        "db read summary"
    );
}

#[cfg(test)]
#[path = "read_volume_tests.rs"]
mod tests;
