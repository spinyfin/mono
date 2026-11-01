//! Once-a-minute rollup of work DB lock contention.
//!
//! `connection_wait` logs every wait/hold of at least 100 ms, which is the
//! right detail for naming one slow caller but makes "is contention building
//! up?" a grep exercise. This module accumulates those events (plus SQLITE_BUSY
//! failures) into a small window and a single timer task flushes it: one
//! `db lock summary` INFO line per minute that saw anything, and the same
//! numbers on the metrics registry.
//!
//! Timing accumulation takes only this module's own tiny mutex, and callers
//! record after releasing the work DB connection, so it never extends a DB
//! critical section. Busy failures are classified by primary result code when
//! a WorkDb method returns and counted in a lock-free atomic. Waiting
//! is SQLite's native `busy_timeout`; nothing here changes timeouts or retry.
use boss_metrics::Registry;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the window is flushed.
pub const SUMMARY_INTERVAL: Duration = Duration::from_secs(60);
const TOP_CALLERS: usize = 5;

boss_metrics::register_counter!(
    DB_LOCK_TIMING_EVENTS,
    "db_lock.timing_events",
    "Work DB connection lock acquisitions whose wait or hold was at least 100 ms."
);
boss_metrics::register_counter!(
    DB_LOCK_BUSY_FAILURES,
    "db_lock.busy_failures",
    "SQLITE_BUSY (\"database is locked\") failures returned by the work DB."
);
boss_metrics::register_gauge!(
    DB_LOCK_LONGEST_HOLD_MS,
    "db_lock.longest_hold_ms",
    "Longest work DB connection hold in the last summary interval (0 when none)."
);

pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&DB_LOCK_TIMING_EVENTS);
    registry.register_counter(&DB_LOCK_BUSY_FAILURES);
    registry.register_gauge(&DB_LOCK_LONGEST_HOLD_MS);
}

#[derive(Default)]
struct Window {
    events: u64,
    longest: Option<(Duration, String)>,
    callers: HashMap<String, (u64, Duration)>,
}

#[derive(Default)]
pub struct LockStats {
    window: Mutex<Window>,
    busy: AtomicU64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CallerTotal {
    pub caller: String,
    pub count: u64,
    pub total_ms: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Summary {
    pub events: u64,
    pub busy: u64,
    pub longest_hold_ms: u64,
    pub longest_caller: String,
    pub top: Vec<CallerTotal>,
}

impl LockStats {
    fn window(&self) -> std::sync::MutexGuard<'_, Window> {
        self.window.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One lock acquisition whose wait or hold crossed the logging threshold.
    pub(crate) fn record_timing(&self, caller: &std::panic::Location<'_>, held: Duration) {
        let caller = caller.to_string();
        let mut window = self.window();
        window.events += 1;
        if window.longest.as_ref().is_none_or(|(longest, _)| held > *longest) {
            window.longest = Some((held, caller.clone()));
        }
        let entry = window.callers.entry(caller).or_default();
        entry.0 += 1;
        entry.1 += held;
    }

    pub(crate) fn record_busy(&self) {
        self.busy.fetch_add(1, Ordering::Relaxed);
    }

    /// Drain the window; `None` when the interval saw no events at all.
    pub(crate) fn take_summary(&self) -> Option<Summary> {
        let window = std::mem::take(&mut *self.window());
        let busy = self.busy.swap(0, Ordering::Relaxed);
        if window.events == 0 && busy == 0 {
            return None;
        }
        let (longest, longest_caller) = window.longest.unwrap_or_default();
        let mut top: Vec<_> = window
            .callers
            .into_iter()
            .map(|(caller, (count, total))| CallerTotal {
                caller,
                count,
                total_ms: total.as_millis() as u64,
            })
            .collect();
        top.sort_by(|a, b| b.total_ms.cmp(&a.total_ms).then_with(|| a.caller.cmp(&b.caller)));
        top.truncate(TOP_CALLERS);
        Some(Summary {
            events: window.events,
            busy,
            longest_hold_ms: longest.as_millis() as u64,
            longest_caller,
            top,
        })
    }
}

pub fn global() -> &'static LockStats {
    static STATS: std::sync::OnceLock<LockStats> = std::sync::OnceLock::new();
    STATS.get_or_init(LockStats::default)
}

/// Drain `stats`, publish to `registry`, and log one line if anything happened.
pub fn flush(stats: &LockStats, registry: &Registry) {
    let summary = stats.take_summary();
    DB_LOCK_LONGEST_HOLD_MS.set(registry, summary.as_ref().map_or(0, |s| s.longest_hold_ms as i64));
    let Some(summary) = summary else { return };
    DB_LOCK_TIMING_EVENTS.inc_by(registry, summary.events);
    DB_LOCK_BUSY_FAILURES.inc_by(registry, summary.busy);
    let top = summary
        .top
        .iter()
        .map(|c| format!("{} count={} total_ms={}", c.caller, c.count, c.total_ms))
        .collect::<Vec<_>>()
        .join("; ");
    tracing::info!(
        interval_secs = SUMMARY_INTERVAL.as_secs(),
        lock_timing_events = summary.events,
        busy_failures = summary.busy,
        longest_hold_ms = summary.longest_hold_ms,
        longest_hold_caller = %summary.longest_caller,
        top_callers = %top,
        "db lock summary"
    );
}

/// Spawn the timer task that flushes the global window every minute.
pub fn spawn_summary_task(registry: Arc<Registry>) -> tokio::task::JoinHandle<()> {
    boss_task_scope::spawn(async move {
        let mut ticker = tokio::time::interval(SUMMARY_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            flush(global(), &registry);
        }
    })
}

/// SQLite's primary result code for "database is locked".
const SQLITE_BUSY: i32 = rusqlite::ffi::SQLITE_BUSY;

/// True when `extended_code` is `SQLITE_BUSY` or any extended code built on it
/// (`SQLITE_BUSY_SNAPSHOT`, `SQLITE_BUSY_RECOVERY`, ...). The primary code is
/// the low byte.
pub(crate) fn is_busy_code(extended_code: i32) -> bool {
    extended_code & 0xff == SQLITE_BUSY
}

/// Classify a returned rusqlite error by its primary result code.
pub(crate) fn is_busy_error(error: &rusqlite::Error) -> bool {
    match error {
        rusqlite::Error::SqliteFailure(err, _) => is_busy_code(err.extended_code),
        _ => false,
    }
}

/// The result boundary for WorkDb methods and independent connection operations.
/// Inspect the returned error after transaction cleanup, never connection state.
/// The typed context prevents recounting when nested WorkDb calls propagate an
/// error; anyhow preserves downcasts to the underlying rusqlite error.
pub fn returned<T>(result: anyhow::Result<T>) -> anyhow::Result<T> {
    returned_with(global(), result)
}

#[derive(Debug)]
struct CountedBusy;

impl std::fmt::Display for CountedBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("work database busy")
    }
}

fn returned_with<T>(stats: &LockStats, result: anyhow::Result<T>) -> anyhow::Result<T> {
    result.map_err(|error| {
        if error.downcast_ref::<CountedBusy>().is_none()
            && error.downcast_ref::<rusqlite::Error>().is_some_and(is_busy_error)
        {
            stats.record_busy();
            error.context(CountedBusy)
        } else {
            error
        }
    })
}

#[cfg(test)]
#[path = "lock_stats_busy_tests.rs"]
mod busy_tests;

#[cfg(test)]
mod tests {
    use super::*;

    static GLOBAL_TEST: Mutex<()> = Mutex::new(());
    use crate::test_support::log_capture;

    #[track_caller]
    fn loc() -> &'static std::panic::Location<'static> {
        std::panic::Location::caller()
    }

    #[test]
    fn summary_counts_events_longest_and_top_callers() {
        let stats = LockStats::default();
        let a = loc();
        let b = loc();
        stats.record_timing(a, Duration::from_millis(150));
        stats.record_timing(a, Duration::from_millis(250));
        stats.record_timing(b, Duration::from_millis(900));
        for _ in 0..7 {
            stats.record_timing(std::panic::Location::caller(), Duration::from_millis(1));
        }
        let summary = stats.take_summary().unwrap();
        assert_eq!(summary.events, 10);
        assert_eq!(summary.longest_hold_ms, 900);
        assert_eq!(summary.longest_caller, b.to_string());
        assert_eq!(
            summary.top[0],
            CallerTotal {
                caller: b.to_string(),
                count: 1,
                total_ms: 900
            }
        );
        assert_eq!(
            summary.top[1],
            CallerTotal {
                caller: a.to_string(),
                count: 2,
                total_ms: 400
            }
        );
        assert!(stats.take_summary().is_none(), "window must reset after a flush");
    }

    #[test]
    fn top_callers_are_capped_at_five() {
        let stats = LockStats::default();
        for line in 0..8u32 {
            let key = format!("f.rs:{line}:1");
            let mut window = stats.window();
            window.events += 1;
            window
                .callers
                .insert(key, (1, Duration::from_millis(u64::from(line) * 10)));
        }
        assert_eq!(stats.take_summary().unwrap().top.len(), 5);
    }

    #[test]
    fn classifies_busy_primary_and_extended_codes() {
        use rusqlite::ffi;
        let busy = |code| {
            is_busy_error(&rusqlite::Error::SqliteFailure(
                ffi::Error {
                    code: rusqlite::ErrorCode::DatabaseBusy,
                    extended_code: code,
                },
                None,
            ))
        };
        assert!(busy(ffi::SQLITE_BUSY));
        assert!(busy(ffi::SQLITE_BUSY_RECOVERY));
        assert!(busy(ffi::SQLITE_BUSY_SNAPSHOT));
        assert!(busy(ffi::SQLITE_BUSY_TIMEOUT));
        assert!(!busy(ffi::SQLITE_LOCKED));
        assert!(!busy(ffi::SQLITE_LOCKED_SHAREDCACHE));
        assert!(!busy(ffi::SQLITE_CONSTRAINT_UNIQUE));
        assert!(!busy(ffi::SQLITE_READONLY));
        assert!(!is_busy_error(&rusqlite::Error::QueryReturnedNoRows));
    }

    fn global_busy() -> u64 {
        global().busy.load(Ordering::Relaxed)
    }

    #[test]
    fn real_writer_busy_failure_reaches_global_flush_and_counter() {
        let _serial = GLOBAL_TEST.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = crate::WorkDb::open(path).unwrap();
        let holder = db.connect_new().unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        db.connect().unwrap().busy_timeout(Duration::from_millis(30)).unwrap();

        let before = global_busy();
        let err = db.set_metadata("k", "v").expect_err("writer is held");
        assert!(format!("{err:#}").contains("locked"), "{err:#}");
        assert!(global_busy() > before, "returned busy must be counted");
        holder.execute_batch("ROLLBACK").unwrap();

        let registry = Registry::new();
        register_metrics(&registry);
        flush(global(), &registry);
        let value = registry
            .counter_snapshots()
            .into_iter()
            .find(|s| s.name == "db_lock.busy_failures")
            .unwrap()
            .value;
        assert!(value >= 1, "global flush must publish the failure, got {value}");
    }

    #[test]
    fn busy_failures_alone_produce_a_summary() {
        let stats = LockStats::default();
        stats.record_busy();
        stats.record_busy();
        let summary = stats.take_summary().unwrap();
        assert_eq!((summary.events, summary.busy, summary.longest_hold_ms), (0, 2, 0));
        assert!(summary.top.is_empty());
    }

    #[test]
    fn flush_logs_and_publishes_metrics_only_for_non_empty_minutes() {
        let buffer = log_capture::install();
        let registry = Registry::new();
        register_metrics(&registry);
        let stats = LockStats::default();
        let count_lines = |from: usize| {
            String::from_utf8_lossy(&buffer.lock()[from..])
                .lines()
                .filter(|l| l.contains("db lock summary"))
                .count()
        };

        let from = buffer.lock().len();
        flush(&stats, &registry);
        assert_eq!(count_lines(from), 0, "empty minute must not log");

        stats.record_timing(loc(), Duration::from_millis(300));
        stats.record_busy();
        let from = buffer.lock().len();
        flush(&stats, &registry);
        assert_eq!(count_lines(from), 1);
        let counter = |name: &str| {
            registry
                .counter_snapshots()
                .into_iter()
                .find(|s| s.name == name)
                .unwrap()
                .value
        };
        assert_eq!(counter("db_lock.timing_events"), 1);
        assert_eq!(counter("db_lock.busy_failures"), 1);
        let gauge = |name: &str| {
            registry
                .gauge_snapshots()
                .into_iter()
                .find(|s| s.name == name)
                .unwrap()
                .value
        };
        assert_eq!(gauge("db_lock.longest_hold_ms"), 300);

        flush(&stats, &registry);
        assert_eq!(gauge("db_lock.longest_hold_ms"), 0);
    }
}
