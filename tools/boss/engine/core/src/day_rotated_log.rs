//! Generic, non-blocking, day-rotated append-only JSONL background writer.
//!
//! Shared by [`crate::ipc_log`] and [`crate::population_timing`]: both need
//! the same shape — fire-and-forget records queued over a channel to a
//! background task that owns a single rotating file handle, rotating and
//! pruning by UTC calendar day. This module owns that machinery once,
//! parameterized over the record type, its filename prefix, and the target
//! directory.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
        let (tx, rx) = mpsc::unbounded_channel();
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(writer_task(dir.into(), file_prefix, rx));
        }
        Self { tx }
    }

    /// Fire-and-forget a record. Dropped silently if the writer task has
    /// exited (receiver gone).
    pub fn emit(&self, rec: T) {
        let _ = self.tx.send(rec);
    }
}

async fn writer_task<T>(dir: PathBuf, file_prefix: &'static str, mut rx: mpsc::UnboundedReceiver<T>)
where
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
            prune_old_files(&dir, file_prefix, RETAIN_DAYS);
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
/// is more than `keep_days` in the past.
pub fn prune_old_files(dir: &Path, file_prefix: &str, keep_days: u64) {
    let cutoff_ms = now_ms().saturating_sub(u128::from(keep_days) * 86_400_000);
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
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

        let old_ms = now_ms().saturating_sub(8 * 86_400_000);
        let old_date = epoch_ms_to_date(old_ms);
        let old_path = dir.path().join(format!("test-{old_date}.jsonl"));
        std::fs::write(&old_path, b"old\n").unwrap();

        let recent_ms = now_ms().saturating_sub(3 * 86_400_000);
        let recent_date = epoch_ms_to_date(recent_ms);
        let recent_path = dir.path().join(format!("test-{recent_date}.jsonl"));
        std::fs::write(&recent_path, b"recent\n").unwrap();

        prune_old_files(dir.path(), "test-", RETAIN_DAYS);

        assert!(!old_path.exists(), "8-day-old file should be pruned");
        assert!(recent_path.exists(), "3-day-old file should be kept");
    }
}
