//! Quiet slow-check reporting.
//!
//! Normal runs print nothing from this module. Only when a single check has
//! been running longer than a threshold does it emit one line naming the
//! check (and the file it is on, when known), repeating at a doubling interval
//! while the check keeps running, and one closing line with the total duration
//! when a reported check finishes.
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
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::output::CheckResult;

use super::ProgressReporter;

/// How long a single check must run before it is reported as slow.
///
/// Chosen from measurement: see `tools/checkleft/docs/slow-check-threshold.md`
/// for the observed peaks and the margin applied.
pub const DEFAULT_SLOW_CHECK_THRESHOLD: Duration = Duration::from_secs(300);

/// How often the watcher thread polls the monitor.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Receives the slow-check lines to print. Lines carry no trailing newline.
pub type SlowCheckSink = Arc<dyn Fn(Vec<String>) + Send + Sync>;

#[derive(Debug)]
struct Slot {
    started_at: Instant,
    total_files: usize,
    /// `Some` once the check has reported per-file progress; until then the
    /// count is unknown, not zero.
    processed_files: Option<usize>,
    /// The file/surface the check last said it was working on.
    active: Option<String>,
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
                processed_files: None,
                active: None,
                reports: 0,
            },
        );
    }

    pub fn record_progress(&mut self, check_id: &str, processed: usize) {
        if let Some(slot) = self.running.get_mut(check_id) {
            slot.processed_files = Some(processed);
        }
    }

    /// The check is working on `label` (a file path or other surface).
    pub fn note_active(&mut self, check_id: &str, label: &str) {
        if let Some(slot) = self.running.get_mut(check_id) {
            slot.active = Some(label.to_owned());
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
            lines.push(format!(
                "checks: still running {check_id}{} ({})",
                describe_position(slot),
                format_elapsed(elapsed)
            ));
        }
        lines
    }
}

/// Where the check is, as precisely as is actually known: the active file when
/// reported, else a processed/total count once progress has been reported,
/// else just how many files it was given (never a misleading `0/N`).
fn describe_position(slot: &Slot) -> String {
    let counts = match (slot.processed_files, slot.total_files) {
        (Some(done), total) if total > 0 => Some(format!("{done}/{total} files")),
        (None, total) if total > 0 => Some(format!("{total} files")),
        _ => None,
    };
    match (&slot.active, counts) {
        (Some(active), Some(counts)) if slot.processed_files.is_some() => format!(" on {active} ({counts})"),
        (Some(active), _) => format!(" on {active}"),
        (None, Some(counts)) => format!(" on {counts}"),
        (None, None) => String::new(),
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    format!("{}s", elapsed.as_secs())
}

/// Decorates a [`ProgressReporter`] with slow-check reporting. Every event is
/// forwarded to the inner reporter unchanged.
pub struct SlowCheckReporter {
    inner: Arc<dyn ProgressReporter>,
    /// Held across both a state transition and delivery of the line it
    /// produced, so a "finished" line can never overtake a "still running"
    /// line computed just before it.
    monitor: Arc<Mutex<SlowCheckMonitor>>,
    sink: SlowCheckSink,
    /// Stop flag + wakeup, so dropping the reporter ends the watcher at once
    /// rather than after the rest of a poll interval.
    stop: Arc<(Mutex<bool>, Condvar)>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl SlowCheckReporter {
    pub fn new(inner: Arc<dyn ProgressReporter>, threshold: Duration, sink: SlowCheckSink) -> Self {
        Self::with_poll_interval(inner, threshold, POLL_INTERVAL, sink)
    }

    fn with_poll_interval(
        inner: Arc<dyn ProgressReporter>,
        threshold: Duration,
        poll_interval: Duration,
        sink: SlowCheckSink,
    ) -> Self {
        let monitor = Arc::new(Mutex::new(SlowCheckMonitor::new(threshold)));
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let handle = {
            let monitor = Arc::clone(&monitor);
            let sink = Arc::clone(&sink);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let (flag, wake) = &*stop;
                loop {
                    let stopped = flag.lock().unwrap();
                    if *stopped {
                        return;
                    }
                    let (stopped, _) = wake.wait_timeout(stopped, poll_interval).unwrap();
                    if *stopped {
                        return;
                    }
                    drop(stopped);
                    let mut monitor = monitor.lock().unwrap();
                    let lines = monitor.poll(Instant::now());
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
        let (flag, wake) = &*self.stop;
        *flag.lock().unwrap() = true;
        wake.notify_all();
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

    fn note_active(&self, check_id: &str, label: &str) {
        self.monitor.lock().unwrap().note_active(check_id, label);
        self.inner.note_active(check_id, label);
    }

    fn finish(&self, check_id: &str, files_failed: usize, elapsed: Duration) {
        {
            let mut monitor = self.monitor.lock().unwrap();
            if let Some(line) = monitor.finish(check_id, Instant::now()) {
                (self.sink)(vec![line]);
            }
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

    #[test]
    fn names_active_file_and_never_claims_zero_progress_when_untracked() {
        let base = Instant::now();
        let mut monitor = SlowCheckMonitor::new(T);
        monitor.register("lint", 7);
        monitor.start("lint", base);
        // No progress and no active file: only the number of files is known.
        assert_eq!(
            monitor.poll(secs(base, 30)),
            vec!["checks: still running lint on 7 files (30s)".to_owned()]
        );
        monitor.note_active("lint", "src/a.rs");
        assert_eq!(
            monitor.poll(secs(base, 60)),
            vec!["checks: still running lint on src/a.rs (60s)".to_owned()]
        );
        monitor.record_progress("lint", 2);
        monitor.note_active("lint", "src/b.rs");
        assert_eq!(
            monitor.poll(secs(base, 120)),
            vec!["checks: still running lint on src/b.rs (2/7 files) (120s)".to_owned()]
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

    fn collecting_sink() -> (SlowCheckSink, Arc<Mutex<Vec<String>>>) {
        let printed: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink: SlowCheckSink = {
            let printed = Arc::clone(&printed);
            Arc::new(move |lines| printed.lock().unwrap().extend(lines))
        };
        (sink, printed)
    }

    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn slow_check_delivers_still_running_then_finished_to_the_sink() {
        let inner = Arc::new(RecordingReporter::default());
        let (sink, printed) = collecting_sink();
        let reporter = SlowCheckReporter::with_poll_interval(
            inner.clone(),
            Duration::from_millis(20),
            Duration::from_millis(5),
            sink,
        );
        reporter.register("lint", 3);
        reporter.start("lint");
        reporter.note_active("lint", "src/a.rs");
        wait_for("slow line", || !printed.lock().unwrap().is_empty());
        reporter.finish("lint", 0, Duration::from_millis(30));
        drop(reporter);

        let lines = printed.lock().unwrap().clone();
        assert!(
            lines[0].starts_with("checks: still running lint on src/a.rs ("),
            "{lines:?}"
        );
        assert!(
            lines.last().unwrap().starts_with("checks: lint finished after "),
            "{lines:?}"
        );
        assert!(inner.events.lock().unwrap().contains(&"finish lint 0".to_owned()));
    }

    #[test]
    fn finish_cannot_overtake_a_still_running_line_already_being_delivered() {
        let inner = Arc::new(RecordingReporter::default());
        let printed: Arc<Mutex<Vec<String>>> = Arc::default();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let tx = Mutex::new(tx);
        // The first delivery announces itself, then lingers; a finish() issued
        // meanwhile must wait for it instead of printing first.
        let sink: SlowCheckSink = {
            let printed = Arc::clone(&printed);
            Arc::new(move |lines| {
                let first = printed.lock().unwrap().is_empty();
                if first {
                    let _ = tx.lock().unwrap().send(());
                    std::thread::sleep(Duration::from_millis(150));
                }
                printed.lock().unwrap().extend(lines);
            })
        };
        let reporter =
            SlowCheckReporter::with_poll_interval(inner, Duration::from_millis(10), Duration::from_millis(5), sink);
        reporter.start("lint");
        rx.recv_timeout(Duration::from_secs(10))
            .expect("slow line delivery began");
        reporter.finish("lint", 0, Duration::from_millis(1));
        drop(reporter);

        let lines = printed.lock().unwrap().clone();
        assert!(lines[0].starts_with("checks: still running lint"), "{lines:?}");
        assert!(lines.last().unwrap().starts_with("checks: lint finished"), "{lines:?}");
    }

    #[test]
    fn drop_does_not_wait_out_the_poll_interval() {
        let (sink, _) = collecting_sink();
        let reporter = SlowCheckReporter::with_poll_interval(
            Arc::new(RecordingReporter::default()),
            DEFAULT_SLOW_CHECK_THRESHOLD,
            Duration::from_secs(60),
            sink,
        );
        let started = Instant::now();
        drop(reporter);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
