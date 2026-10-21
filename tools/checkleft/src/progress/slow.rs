//! Quiet slow-check reporting.
//!
//! Normal runs print nothing from this module. Only when a single check has
//! been running longer than a threshold does it emit one line naming the
//! check, repeating at a doubling interval while the check keeps running, and
//! one closing line with the total duration when a reported check finishes.
//!
//! This is reporting only: it never times out, kills, or otherwise affects a
//! check, so findings and exit codes are unchanged.
//!
//! [`SlowCheckMonitor`] is the pure model — every method takes `now` as an
//! argument, so tests drive the clock deterministically with no real sleeps.
//! [`SlowCheckReporter`] adapts it to [`ProgressReporter`] as a decorator over
//! whichever reporter the run already uses, plus a watcher thread that polls
//! the monitor on the real clock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::output::CheckResult;

use super::ProgressReporter;

/// How long a single check must run before it is reported as slow.
///
/// Chosen from measurement: see `tools/checkleft/docs/slow-check-threshold.md`
/// for the observed per-check peaks and the margin applied.
pub const DEFAULT_SLOW_CHECK_THRESHOLD: Duration = Duration::from_secs(180);

/// How often the watcher thread polls the monitor.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Receives the slow-check lines to print. Lines carry no trailing newline.
pub type SlowCheckSink = Arc<dyn Fn(Vec<String>) + Send + Sync>;

#[derive(Debug)]
struct Slot {
    started_at: Instant,
    total_files: usize,
    processed_files: usize,
    /// How many "still running" lines have been emitted for this run.
    reports: u32,
}

/// Pure slow-check model; see the module docs.
#[derive(Debug)]
pub struct SlowCheckMonitor {
    threshold: Duration,
    totals: HashMap<String, usize>,
    running: HashMap<String, Slot>,
}

impl SlowCheckMonitor {
    pub fn new(threshold: Duration) -> Self {
        Self {
            threshold,
            totals: HashMap::new(),
            running: HashMap::new(),
        }
    }

    pub fn register(&mut self, check_id: &str, total_files: usize) {
        self.totals.insert(check_id.to_owned(), total_files);
    }

    pub fn start(&mut self, check_id: &str, now: Instant) {
        let total_files = self.totals.get(check_id).copied().unwrap_or(0);
        self.running.insert(
            check_id.to_owned(),
            Slot {
                started_at: now,
                total_files,
                processed_files: 0,
                reports: 0,
            },
        );
    }

    pub fn record_progress(&mut self, check_id: &str, processed: usize) {
        if let Some(slot) = self.running.get_mut(check_id) {
            slot.processed_files = processed;
        }
    }

    /// The check finished. Returns the completion line only if the check had
    /// previously been reported as slow.
    pub fn finish(&mut self, check_id: &str, now: Instant) -> Option<String> {
        let slot = self.running.remove(check_id)?;
        (slot.reports > 0).then(|| {
            format!(
                "checks: {check_id} finished after {}",
                format_elapsed(now.saturating_duration_since(slot.started_at))
            )
        })
    }

    /// Lines due at `now`. A check is first reported once it has run for
    /// `threshold`, then again at 2x, 4x, ... the threshold (the interval
    /// between reports doubles).
    pub fn poll(&mut self, now: Instant) -> Vec<String> {
        let mut due: Vec<(&String, &mut Slot)> = self.running.iter_mut().collect();
        due.sort_by(|a, b| a.0.cmp(b.0));
        let mut lines = Vec::new();
        for (check_id, slot) in due {
            let elapsed = now.saturating_duration_since(slot.started_at);
            let next_report_at = self.threshold.saturating_mul(1u32 << slot.reports.min(20));
            if elapsed < next_report_at {
                continue;
            }
            slot.reports += 1;
            let files = if slot.total_files > 0 {
                format!(" on {}/{} files", slot.processed_files, slot.total_files)
            } else {
                String::new()
            };
            lines.push(format!(
                "checks: still running {check_id}{files} ({})",
                format_elapsed(elapsed)
            ));
        }
        lines
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    format!("{}s", elapsed.as_secs())
}

/// Decorates a [`ProgressReporter`] with slow-check reporting. Every event is
/// forwarded to the inner reporter unchanged.
pub struct SlowCheckReporter {
    inner: Arc<dyn ProgressReporter>,
    monitor: Arc<Mutex<SlowCheckMonitor>>,
    sink: SlowCheckSink,
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl SlowCheckReporter {
    pub fn new(inner: Arc<dyn ProgressReporter>, threshold: Duration, sink: SlowCheckSink) -> Self {
        let monitor = Arc::new(Mutex::new(SlowCheckMonitor::new(threshold)));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let monitor = Arc::clone(&monitor);
            let sink = Arc::clone(&sink);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(POLL_INTERVAL);
                    let lines = monitor.lock().unwrap().poll(Instant::now());
                    if !lines.is_empty() {
                        sink(lines);
                    }
                }
            })
        };
        Self {
            inner,
            monitor,
            sink,
            stop,
            handle: Mutex::new(Some(handle)),
        }
    }
}

impl Drop for SlowCheckReporter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

impl ProgressReporter for SlowCheckReporter {
    fn register(&self, check_id: &str, total_files: usize) {
        self.monitor.lock().unwrap().register(check_id, total_files);
        self.inner.register(check_id, total_files);
    }

    fn start(&self, check_id: &str) {
        self.monitor.lock().unwrap().start(check_id, Instant::now());
        self.inner.start(check_id);
    }

    fn start_fix(&self, check_id: &str, pass: u32) {
        self.monitor.lock().unwrap().start(check_id, Instant::now());
        self.inner.start_fix(check_id, pass);
    }

    fn record_progress(&self, check_id: &str, processed: usize) {
        self.monitor.lock().unwrap().record_progress(check_id, processed);
        self.inner.record_progress(check_id, processed);
    }

    fn finish(&self, check_id: &str, files_failed: usize, elapsed: Duration) {
        let line = self.monitor.lock().unwrap().finish(check_id, Instant::now());
        if let Some(line) = line {
            (self.sink)(vec![line]);
        }
        self.inner.finish(check_id, files_failed, elapsed);
    }

    fn stream_findings(&self, result: &CheckResult) {
        self.inner.stream_findings(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(30);

    fn secs(base: Instant, s: u64) -> Instant {
        base + Duration::from_secs(s)
    }

    #[test]
    fn silent_below_threshold_and_on_fast_finish() {
        let base = Instant::now();
        let mut monitor = SlowCheckMonitor::new(T);
        monitor.register("typo", 4);
        monitor.start("typo", base);
        assert!(monitor.poll(secs(base, 1)).is_empty());
        assert!(monitor.poll(secs(base, 29)).is_empty());
        assert_eq!(monitor.finish("typo", secs(base, 29)), None);
    }

    #[test]
    fn reports_at_threshold_with_doubling_backoff_then_completion() {
        let base = Instant::now();
        let mut monitor = SlowCheckMonitor::new(T);
        monitor.register("typo", 12);
        monitor.start("typo", base);
        monitor.record_progress("typo", 3);

        assert_eq!(
            monitor.poll(secs(base, 30)),
            vec!["checks: still running typo on 3/12 files (30s)".to_owned()]
        );
        // Same instant and anything before the doubled interval: silent.
        assert!(monitor.poll(secs(base, 30)).is_empty());
        assert!(monitor.poll(secs(base, 59)).is_empty());
        assert_eq!(
            monitor.poll(secs(base, 60)),
            vec!["checks: still running typo on 3/12 files (60s)".to_owned()]
        );
        assert!(monitor.poll(secs(base, 119)).is_empty());
        assert_eq!(
            monitor.poll(secs(base, 120)),
            vec!["checks: still running typo on 3/12 files (120s)".to_owned()]
        );
        assert_eq!(
            monitor.finish("typo", secs(base, 150)),
            Some("checks: typo finished after 150s".to_owned())
        );
        // Finished checks are not reported again.
        assert!(monitor.poll(secs(base, 600)).is_empty());
    }

    #[test]
    fn concurrent_checks_report_independently() {
        let base = Instant::now();
        let mut monitor = SlowCheckMonitor::new(T);
        monitor.start("fast", base);
        monitor.start("slow", base);
        assert_eq!(monitor.finish("fast", secs(base, 2)), None);
        monitor.start("late", secs(base, 20));
        assert_eq!(
            monitor.poll(secs(base, 30)),
            vec!["checks: still running slow (30s)".to_owned()]
        );
        assert_eq!(
            monitor.poll(secs(base, 50)),
            vec!["checks: still running late (30s)".to_owned()]
        );
    }

    #[derive(Default)]
    struct RecordingReporter {
        events: Mutex<Vec<String>>,
    }

    impl ProgressReporter for RecordingReporter {
        fn register(&self, id: &str, n: usize) {
            self.events.lock().unwrap().push(format!("register {id} {n}"));
        }
        fn start(&self, id: &str) {
            self.events.lock().unwrap().push(format!("start {id}"));
        }
        fn start_fix(&self, id: &str, pass: u32) {
            self.events.lock().unwrap().push(format!("start_fix {id} {pass}"));
        }
        fn record_progress(&self, id: &str, n: usize) {
            self.events.lock().unwrap().push(format!("progress {id} {n}"));
        }
        fn finish(&self, id: &str, failed: usize, _elapsed: Duration) {
            self.events.lock().unwrap().push(format!("finish {id} {failed}"));
        }
        fn stream_findings(&self, result: &CheckResult) {
            self.events.lock().unwrap().push(format!("stream {}", result.check_id));
        }
    }

    #[test]
    fn fast_run_forwards_events_unchanged_and_prints_nothing() {
        let inner = Arc::new(RecordingReporter::default());
        let printed: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink: SlowCheckSink = {
            let printed = Arc::clone(&printed);
            Arc::new(move |lines| printed.lock().unwrap().extend(lines))
        };
        let reporter = SlowCheckReporter::new(inner.clone(), DEFAULT_SLOW_CHECK_THRESHOLD, sink);

        reporter.register("typo", 2);
        reporter.start("typo");
        reporter.record_progress("typo", 2);
        reporter.stream_findings(&CheckResult {
            check_id: "typo".to_owned(),
            findings: vec![],
        });
        reporter.finish("typo", 0, Duration::from_millis(5));
        drop(reporter);

        assert!(printed.lock().unwrap().is_empty());
        assert_eq!(
            *inner.events.lock().unwrap(),
            vec![
                "register typo 2",
                "start typo",
                "progress typo 2",
                "stream typo",
                "finish typo 0"
            ]
        );
    }
}
