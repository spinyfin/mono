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
//! critical section. Busy failures are counted in a lock-free atomic from the
//! connection's busy handler, so that path never takes any lock.
use boss_metrics::Registry;
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the window is flushed.
pub(crate) const SUMMARY_INTERVAL: Duration = Duration::from_secs(60);
const TOP_CALLERS: usize = 5;

crate::register_counter!(
    DB_LOCK_TIMING_EVENTS,
    "db_lock.timing_events",
    "Work DB connection lock acquisitions whose wait or hold was at least 100 ms."
);
crate::register_counter!(
    DB_LOCK_BUSY_FAILURES,
    "db_lock.busy_failures",
    "SQLITE_BUSY (\"database is locked\") failures reported by SQLite."
);
crate::register_gauge!(
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
pub(crate) struct LockStats {
    window: Mutex<Window>,
    busy: AtomicU64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CallerTotal {
    pub caller: String,
    pub count: u64,
    pub total_ms: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Summary {
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

pub(crate) fn global() -> &'static LockStats {
    static STATS: std::sync::OnceLock<LockStats> = std::sync::OnceLock::new();
    STATS.get_or_init(LockStats::default)
}

/// Drain `stats`, publish to `registry`, and log one line if anything happened.
pub(crate) fn flush(stats: &LockStats, registry: &Registry) {
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
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SUMMARY_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            flush(global(), &registry);
        }
    })
}

#[cfg(test)]
thread_local! {
    /// Test-only per-thread override of the busy limit, in milliseconds.
    /// SQLite invokes the busy handler on the calling thread, so this lets a
    /// test use a short limit without affecting concurrently running tests.
    static LIMIT_OVERRIDE_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

fn busy_limit_ms() -> u64 {
    #[cfg(test)]
    if let Some(ms) = LIMIT_OVERRIDE_MS.with(std::cell::Cell::get) {
        return ms;
    }
    super::SQLITE_BUSY_TIMEOUT.as_millis() as u64
}

/// SQLite's own `busy_timeout` backoff schedule (`sqliteDefaultBusyCallback`),
/// reproduced so replacing the handler leaves waiting behaviour unchanged.
const DELAYS_MS: [u64; 12] = [1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100];
const TOTALS_MS: [u64; 12] = [0, 1, 3, 8, 18, 33, 53, 78, 103, 128, 178, 228];

/// Milliseconds to sleep before retrying invocation `n`, or `None` once the
/// `limit_ms` budget is spent (SQLite then returns SQLITE_BUSY to the caller).
fn next_delay_ms(n: u64, limit_ms: u64) -> Option<u64> {
    let (delay, prior) = if (n as usize) < DELAYS_MS.len() {
        (DELAYS_MS[n as usize], TOTALS_MS[n as usize])
    } else {
        (100, 228 + 100 * (n - 11))
    };
    let delay = delay.min(limit_ms.saturating_sub(prior));
    (delay > 0).then_some(delay)
}

fn busy_handler(n: i32) -> bool {
    match next_delay_ms(n.max(0) as u64, busy_limit_ms()) {
        Some(delay) => {
            std::thread::sleep(Duration::from_millis(delay));
            true
        }
        None => {
            // The wait is exhausted: SQLite now returns SQLITE_BUSY ("database
            // is locked") for this statement, including a timed-out
            // `BEGIN IMMEDIATE`. Count exactly that, once per failure.
            global().record_busy();
            false
        }
    }
}

/// Install the counting busy handler on a work DB connection. It waits exactly
/// as long as `busy_timeout(SQLITE_BUSY_TIMEOUT)` would, and counts each
/// failure it gives up on. Must be called instead of `busy_timeout`.
pub(crate) fn install_busy_counter(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_handler(Some(busy_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn delay_schedule_matches_sqlite_default_and_respects_limit() {
        let total = |limit| (0..).map_while(|n| next_delay_ms(n, limit)).sum::<u64>();
        assert_eq!(next_delay_ms(0, 5000), Some(1));
        assert_eq!(next_delay_ms(11, 5000), Some(100));
        assert_eq!(next_delay_ms(12, 5000), Some(100));
        assert_eq!(total(5000), 5000);
        assert_eq!(total(250), 250);
        assert_eq!(total(2), 2);
        assert_eq!(next_delay_ms(0, 0), None);
    }

    #[test]
    fn real_busy_failure_reaches_summary_and_counter() {
        use rusqlite::{ErrorCode, TransactionBehavior};
        LIMIT_OVERRIDE_MS.with(|l| l.set(Some(30)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("busy.db");
        let mut holder = Connection::open(&path).unwrap();
        holder
            .execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t(x);")
            .unwrap();
        let mut waiter = Connection::open(&path).unwrap();
        install_busy_counter(&waiter).unwrap();
        waiter.set_transaction_behavior(TransactionBehavior::Immediate);
        let _ = global().take_summary();

        let held = holder
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        // The contended case: `BEGIN IMMEDIATE` timing out.
        let err = waiter.transaction().expect_err("must fail while the writer is held");
        assert_eq!(err.sqlite_error_code(), Some(ErrorCode::DatabaseBusy), "{err:?}");
        drop(held);
        LIMIT_OVERRIDE_MS.with(|l| l.set(None));

        let registry = Registry::new();
        register_metrics(&registry);
        let summary = global().take_summary().expect("busy failure must produce a summary");
        assert_eq!(summary.busy, 1);
        // Feed the drained window through flush's publishing path.
        let stats = LockStats::default();
        stats.busy.store(summary.busy, Ordering::Relaxed);
        flush(&stats, &registry);
        let value = registry
            .counter_snapshots()
            .into_iter()
            .find(|s| s.name == "db_lock.busy_failures")
            .unwrap()
            .value;
        assert_eq!(value, 1);

        // A successful wait (writer released) is not a failure.
        waiter.transaction().unwrap().commit().unwrap();
        assert!(global().take_summary().is_none_or(|s| s.busy == 0));
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
