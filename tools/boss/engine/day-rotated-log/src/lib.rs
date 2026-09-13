//! Generic, non-blocking, day-rotated append-only JSONL background writer.
//!
//! Shared by the engine's `ipc_log` and `population_timing` modules: both
//! need the same shape — fire-and-forget records queued over a channel to a
//! background task that owns a single rotating file handle, rotating and
//! pruning by UTC calendar day. This module owns that machinery once,
//! parameterized over the record type, its filename prefix, and the target
//! directory.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio::sync::mpsc;

/// Default days of history retained before a rotated file is pruned.
pub const RETAIN_DAYS: u64 = 7;

/// A record that carries its own wall-clock timestamp, used to decide which
/// day file it belongs in.
pub trait TimestampedRecord {
    fn ts_epoch_ms(&self) -> u128;
}

/// Async-safe, append-only, day-rotated log writer for records of type `T`.
///
/// Calls to [`emit`](Self::emit) are non-blocking: entries are sent over an
/// in-process channel to a background task that owns the file handle and
/// performs all I/O.
pub struct DayRotatedLogger<T> {
    tx: mpsc::UnboundedSender<T>,
}

impl<T> DayRotatedLogger<T>
where
    T: Serialize + TimestampedRecord + Send + 'static,
{
    /// Create a logger that writes day files named `<file_prefix><date>.jsonl`
    /// directly under `dir`. Spawns the background writer task when a Tokio
    /// runtime is available. When created outside a runtime (e.g.
    /// synchronous unit tests), the channel is created but the writer task is
    /// not spawned — entries queue up and are silently dropped when the
    /// sender is dropped.
    pub fn new(dir: impl Into<PathBuf>, file_prefix: &'static str) -> Self {
        Self::with_clock(dir, file_prefix, now_ms)
    }

    /// [`new`](Self::new) with an explicit clock. The clock decides the
    /// prune cutoff on each day rollover; production passes [`now_ms`] and
    /// tests pass a fixed instant so their fixtures never age relative to
    /// the real calendar.
    fn with_clock(dir: impl Into<PathBuf>, file_prefix: &'static str, clock: fn() -> u128) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(writer_task(dir.into(), file_prefix, rx, clock));
        }
        Self { tx }
    }

    /// Fire-and-forget a record. Dropped silently if the writer task has
    /// exited (receiver gone).
    pub fn emit(&self, rec: T) {
        let _ = self.tx.send(rec);
    }
}

async fn writer_task<T>(
    dir: PathBuf,
    file_prefix: &'static str,
    mut rx: mpsc::UnboundedReceiver<T>,
    clock: fn() -> u128,
) where
    T: Serialize + TimestampedRecord,
{
    use std::io::Write;

    let mut current_date = String::new();
    let mut file: Option<std::fs::File> = None;

    while let Some(rec) = rx.recv().await {
        let date_str = epoch_ms_to_date(rec.ts_epoch_ms());

        if date_str != current_date {
            // Date rolled over: close the old file and prune old logs.
            file = None;
            prune_old_files_at(&dir, file_prefix, RETAIN_DAYS, clock());
        }

        if file.is_none() {
            if let Err(err) = std::fs::create_dir_all(&dir) {
                tracing::warn!(
                    ?err,
                    prefix = file_prefix,
                    "day_rotated_log: failed to create log dir; dropping entry"
                );
                continue;
            }
            let path = dir.join(format!("{file_prefix}{date_str}.jsonl"));
            match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => {
                    file = Some(f);
                    current_date = date_str;
                }
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        path = %path.display(),
                        "day_rotated_log: failed to open log file; dropping entry"
                    );
                    continue;
                }
            }
        }

        let Some(ref mut f) = file else { continue };
        match serde_json::to_vec(&rec) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                if let Err(err) = f.write_all(&bytes) {
                    tracing::warn!(?err, "day_rotated_log: write failed; dropping entry");
                }
            }
            Err(err) => {
                tracing::warn!(?err, "day_rotated_log: serialization failed; dropping entry");
            }
        }
    }
}

/// Remove day files under `dir` named `<file_prefix><date>.jsonl` whose date
/// is more than `keep_days` before now.
pub fn prune_old_files(dir: &Path, file_prefix: &str, keep_days: u64) {
    prune_old_files_at(dir, file_prefix, keep_days, now_ms());
}

/// [`prune_old_files`] judged against an explicit `now_epoch_ms` instead of
/// the live clock: a day file is removed when its date is more than
/// `keep_days` before the UTC calendar day containing `now_epoch_ms`.
pub fn prune_old_files_at(dir: &Path, file_prefix: &str, keep_days: u64, now_epoch_ms: u128) {
    let cutoff_ms = now_epoch_ms.saturating_sub(u128::from(keep_days) * 86_400_000);
    let cutoff_date = epoch_ms_to_date(cutoff_ms);

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(date_part) = name.strip_prefix(file_prefix).and_then(|s| s.strip_suffix(".jsonl"))
            && date_part < cutoff_date.as_str()
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub fn now_ms() -> u128 {
    boss_engine_utils::epoch_time::now_epoch_ms()
}

/// UTC `YYYY-MM-DD` for an epoch-millis instant.
pub fn epoch_ms_to_date(ms: u128) -> String {
    match chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64) {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => "1970-01-01".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: u128 = 86_400_000;

    /// Fixed "now" for every prune test: 2026-05-14T12:00:00Z. Fixture
    /// dates are derived from it so nothing here depends on the real
    /// calendar or on the UTC day rolling over mid-test.
    const NOW_MS: u128 = 1_778_716_800_000 + 43_200_000;

    fn fixed_now_ms() -> u128 {
        NOW_MS
    }

    #[test]
    fn epoch_ms_to_date_known_values() {
        // 2026-05-14 00:00:00 UTC = 1 778 716 800 seconds
        let ms = 1_778_716_800_000u128;
        assert_eq!(epoch_ms_to_date(ms), "2026-05-14");
        assert_eq!(epoch_ms_to_date(ms + 43_200_000), "2026-05-14"); // noon same day
        assert_eq!(epoch_ms_to_date(ms + 86_400_000), "2026-05-15"); // next day
    }

    #[test]
    fn prune_old_files_removes_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();

        let old_date = epoch_ms_to_date(NOW_MS - 8 * DAY_MS);
        let old_path = dir.path().join(format!("test-{old_date}.jsonl"));
        std::fs::write(&old_path, b"old\n").unwrap();

        let recent_date = epoch_ms_to_date(NOW_MS - 3 * DAY_MS);
        let recent_path = dir.path().join(format!("test-{recent_date}.jsonl"));
        std::fs::write(&recent_path, b"recent\n").unwrap();

        prune_old_files_at(dir.path(), "test-", RETAIN_DAYS, NOW_MS);

        assert!(!old_path.exists(), "8-day-old file should be pruned");
        assert!(recent_path.exists(), "3-day-old file should be kept");
    }

    #[test]
    fn epoch_ms_to_date_epoch_and_out_of_range_fall_back_to_1970() {
        // The literal epoch instant renders as its true date, not the fallback.
        assert_eq!(epoch_ms_to_date(0), "1970-01-01");

        // i64::MAX ms is ~292 million years past the epoch — far outside the
        // range chrono can represent, so `from_timestamp_millis` returns None
        // and we hit the fallback arm.
        assert_eq!(epoch_ms_to_date(i64::MAX as u128), "1970-01-01");
    }

    #[test]
    fn prune_old_files_leaves_other_prefixes_untouched() {
        let dir = tempfile::TempDir::new().unwrap();

        let old_date = epoch_ms_to_date(NOW_MS - 30 * DAY_MS);
        let mine = dir.path().join(format!("test-{old_date}.jsonl"));
        let theirs = dir.path().join(format!("other-{old_date}.jsonl"));
        std::fs::write(&mine, b"mine\n").unwrap();
        std::fs::write(&theirs, b"theirs\n").unwrap();

        prune_old_files_at(dir.path(), "test-", RETAIN_DAYS, NOW_MS);

        assert!(!mine.exists(), "stale file with matching prefix is pruned");
        assert!(theirs.exists(), "stale file with a different prefix must be left alone");
    }

    #[test]
    fn prune_old_files_ignores_non_jsonl_suffixes() {
        let dir = tempfile::TempDir::new().unwrap();

        let old_date = epoch_ms_to_date(NOW_MS - 30 * DAY_MS);
        // An in-progress temp file: stale-dated and matching prefix, but not
        // a completed `.jsonl` — the suffix guard must protect it.
        let tmp = dir.path().join(format!("test-{old_date}.tmp"));
        let jsonl = dir.path().join(format!("test-{old_date}.jsonl"));
        std::fs::write(&tmp, b"partial\n").unwrap();
        std::fs::write(&jsonl, b"done\n").unwrap();

        prune_old_files_at(dir.path(), "test-", RETAIN_DAYS, NOW_MS);

        assert!(!jsonl.exists(), "stale .jsonl file is pruned");
        assert!(tmp.exists(), "stale non-.jsonl file must be left alone");
    }

    #[test]
    fn prune_old_files_keeps_the_cutoff_date_but_drops_a_day_older() {
        let dir = tempfile::TempDir::new().unwrap();

        // The prune comparison is strict (`date_part < cutoff_date`), so a file
        // dated exactly on the cutoff boundary is retained while one a single
        // day older is removed.
        let cutoff_ms = NOW_MS - u128::from(RETAIN_DAYS) * DAY_MS;
        let cutoff_date = epoch_ms_to_date(cutoff_ms);
        let older_date = epoch_ms_to_date(cutoff_ms - DAY_MS);
        assert_ne!(cutoff_date, older_date, "boundary dates must differ");

        let on_boundary = dir.path().join(format!("test-{cutoff_date}.jsonl"));
        let past_boundary = dir.path().join(format!("test-{older_date}.jsonl"));
        std::fs::write(&on_boundary, b"boundary\n").unwrap();
        std::fs::write(&past_boundary, b"older\n").unwrap();

        prune_old_files_at(dir.path(), "test-", RETAIN_DAYS, NOW_MS);

        assert!(
            on_boundary.exists(),
            "file dated on the cutoff must be kept (comparison is strict)"
        );
        assert!(
            !past_boundary.exists(),
            "file one day older than the cutoff must be pruned"
        );
    }

    #[test]
    fn prune_old_files_on_missing_dir_is_a_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        // Must not panic — the read_dir error is swallowed with an early return.
        prune_old_files(&missing, "test-", RETAIN_DAYS);
        assert!(!missing.exists());
    }

    /// Minimal record exercising the generic writer: carries an explicit
    /// epoch-millis timestamp so the day it lands in is fully deterministic.
    #[derive(serde::Serialize)]
    struct TestRec {
        ts_epoch_ms: u128,
        msg: &'static str,
    }

    impl TimestampedRecord for TestRec {
        fn ts_epoch_ms(&self) -> u128 {
            self.ts_epoch_ms
        }
    }

    #[tokio::test]
    async fn writer_task_rotates_across_days_and_prunes_on_rollover() {
        let dir = tempfile::TempDir::new().unwrap();

        // Pre-seed a stale file (well older than RETAIN_DAYS relative to the
        // writer's fixed clock) that the prune triggered by the first
        // rollover must remove.
        let stale_date = epoch_ms_to_date(NOW_MS - 30 * DAY_MS);
        let stale = dir.path().join(format!("test-{stale_date}.jsonl"));
        std::fs::write(&stale, b"stale\n").unwrap();

        // Two explicit instants on consecutive UTC calendar days: "yesterday"
        // and "today" relative to the writer's fixed clock, so neither day
        // file is ever a prune candidate and nothing depends on when this
        // test runs.
        let day1_ms = NOW_MS - DAY_MS;
        let day2_ms = NOW_MS;
        let date1 = epoch_ms_to_date(day1_ms);
        let date2 = epoch_ms_to_date(day2_ms);
        assert_ne!(date1, date2, "the two records must fall on different days");

        let logger = DayRotatedLogger::with_clock(dir.path(), "test-", fixed_now_ms);
        logger.emit(TestRec {
            ts_epoch_ms: day1_ms,
            msg: "one",
        });
        logger.emit(TestRec {
            ts_epoch_ms: day2_ms,
            msg: "two",
        });

        let file1 = dir.path().join(format!("test-{date1}.jsonl"));
        let file2 = dir.path().join(format!("test-{date2}.jsonl"));

        // Poll for the background writer to flush both files. Generous bound to
        // tolerate slow CI runners.
        for _ in 0..200 {
            if file1.exists() && file2.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert!(file1.exists(), "first day's file should be written");
        assert!(file2.exists(), "second day's file should be written (rotation)");
        assert!(
            !stale.exists(),
            "stale pre-seeded file should be pruned when the rollover fires"
        );
    }
}
