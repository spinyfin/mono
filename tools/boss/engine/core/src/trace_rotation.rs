//! Rotation and retention for `engine-trace.jsonl`.
//!
//! Rotation is **purely size-based**: [`RotatingJsonlWriter`] checks the
//! running byte count after every write and rotates to a timestamped
//! backup (`engine-trace.jsonl.<unix_s>`) when the threshold is crossed.
//! [`rotate_on_startup`] applies that same size check once at process
//! start — it rotates only if the existing file is already at or over
//! the threshold, so an engine restart never rotates a file that isn't
//! full, and a restart storm cannot burn through the retention budget on
//! its own (thirteen restarts in under eleven minutes previously
//! evicted the entire retention window because every start rotated
//! unconditionally). A restart simply keeps appending to whatever active
//! file it finds, exactly like a write from within the same process
//! would.
//!
//! Rotated files are pruned to the N most recent; older files are
//! deleted automatically.
//!
//! ## Configuration (env overrides; defaults are safe without any config)
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `BOSS_ENGINE_TRACE_MAX_BYTES` | `104857600` (100 MiB) | Rotate when file exceeds this size |
//! | `BOSS_ENGINE_TRACE_MAX_FILES` | `10` | Keep at most this many rotated backups |
//!
//! Retention is `max_files` × `max_bytes` of trace volume (default
//! 10 × 100 MiB, plus one active file → worst case ~1.1 GiB on disk).
//! Wall-clock coverage depends on write rate; at observed volumes
//! (tens of MB across a multi-day corpus under the old restart-cut
//! regime, so far below the size threshold) a single 100 MiB segment
//! is expected to span many days, and the full 11-file window well
//! over a week. Do not treat older "N segments spanned M days"
//! measurements taken under unconditional rotate-on-start as if they
//! characterised size-based segments.
//!
//! ## Rotation safety
//!
//! Rotation happens while the writer's mutex is held, so no concurrent
//! write can race with the rename + re-open.  On Unix, an open file
//! descriptor follows the inode through a rename, so any unflushed bytes
//! already written land safely in the renamed file before the old `File`
//! handle is dropped.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use crate::rotating_file::{self, RotatingFileWriter};
use boss_log_files::next_rotated_path;
#[cfg(test)]
use boss_log_files::rotated_segments;

pub const TRACE_MAX_BYTES_ENV: &str = "BOSS_ENGINE_TRACE_MAX_BYTES";
pub const TRACE_MAX_FILES_ENV: &str = "BOSS_ENGINE_TRACE_MAX_FILES";

/// Default maximum size before rotation: 100 MiB.
pub const DEFAULT_TRACE_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// Default number of rotated backups to keep. Sized for volume-based
/// retention (see module docs), not for the number of times the engine
/// has restarted.
pub const DEFAULT_TRACE_MAX_FILES: usize = 10;

/// Read rotation config from env vars, falling back to safe defaults.
pub fn trace_rotation_config() -> (u64, usize) {
    let max_bytes = boss_engine_utils::env_parse::env_parsed_or(TRACE_MAX_BYTES_ENV, DEFAULT_TRACE_MAX_BYTES);
    let max_files = boss_engine_utils::env_parse::env_parsed_or(TRACE_MAX_FILES_ENV, DEFAULT_TRACE_MAX_FILES);
    (max_bytes, max_files)
}

/// Called once at engine startup before opening the trace file.
///
/// Applies the same size threshold [`RotatingJsonlWriter`] uses mid-run:
/// if `path` exists and is already at or over `max_bytes`, it is renamed
/// to a timestamped backup and old backups are pruned to `max_files`. If
/// the file exists but is under the threshold, this is a no-op — the
/// engine keeps appending to it, exactly as a live process would. This
/// is what stops a restart storm from evicting retained history: a
/// restart no longer rotates by itself, only genuine size growth does.
/// Any error is printed to stderr and swallowed — trace rotation must
/// never block engine startup.
pub fn rotate_on_startup(path: &Path, max_bytes: u64, max_files: usize) {
    let size = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return,
        Err(err) => {
            eprintln!("boss-engine: could not stat engine-trace.jsonl on startup: {err}");
            return;
        }
    };
    if size < max_bytes {
        return;
    }
    let rotated = next_rotated_path(path);
    if let Err(err) = std::fs::rename(path, &rotated) {
        eprintln!("boss-engine: could not rotate engine-trace.jsonl on startup: {err}");
        return;
    }
    prune_old_rotated(path, max_files);
}

/// Open (or create) the trace file for appending.  The directory is
/// created if needed.  Called both at startup and after each rotation.
pub fn open_trace_file(path: &Path) -> io::Result<File> {
    rotating_file::open_append_file(path)
}

/// Delete the oldest rotated backups, keeping at most `max_files`.
/// Silently ignores any deletion error — this is best-effort cleanup.
///
/// [`rotated_segments`] returns the `<base>.<unix_seconds>` files oldest-first
/// (ascending timestamp), so the oldest `len - max_files` are simply the
/// leading slice. The rotated-segment format and ordering both live in
/// `boss-log-files` — this writer never re-encodes them.
pub fn prune_old_rotated(active_path: &Path, max_files: usize) {
    rotating_file::prune_old_rotated(active_path, max_files, "trace file")
}

pub use crate::rotating_file::RotatingState;

/// `Write` impl for `engine-trace.jsonl` that rotates the file when the
/// byte threshold is crossed.
///
/// This is a thin JSONL-specific wrapper around [`RotatingFileWriter`]. Its
/// inner writer rotates after complete records are written, so a JSON line is
/// never split across segments.
pub struct RotatingJsonlWriter {
    inner: RotatingFileWriter,
}

impl RotatingJsonlWriter {
    pub fn new(inner: RotatingFileWriter) -> Self {
        Self { inner }
    }
}

impl Write for RotatingJsonlWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Write::write(&mut self.inner, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn tmp_trace(dir: &TempDir) -> PathBuf {
        dir.path().join("engine-trace.jsonl")
    }

    fn writer(
        path: PathBuf,
        state: Arc<Mutex<Option<RotatingState>>>,
        max_bytes: u64,
        max_files: usize,
    ) -> RotatingJsonlWriter {
        RotatingJsonlWriter::new(RotatingFileWriter {
            path,
            state,
            max_bytes,
            max_files,
            log_name: "trace file",
            rotate_after_write: true,
        })
    }

    #[test]
    fn rotate_on_startup_renames_file_at_or_over_threshold() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        fs::write(&path, b"0123456789").unwrap(); // 10 bytes

        rotate_on_startup(&path, 10, 5);

        assert!(!path.exists(), "active path should be gone after startup rotation");
        let backups = rotated_segments(&path);
        assert_eq!(backups.len(), 1, "expected one rotated backup");
    }

    #[test]
    fn rotate_on_startup_leaves_file_under_threshold_alone() {
        // A restart with an active file that hasn't hit the size threshold
        // must not consume a rotation slot — otherwise a restart storm
        // burns through retention on its own.
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        fs::write(&path, b"line1\n").unwrap();

        rotate_on_startup(&path, 100, 5);

        assert!(path.exists(), "active path should be left in place below threshold");
        assert!(rotated_segments(&path).is_empty(), "no rotation should have happened");
    }

    #[test]
    fn restart_storm_does_not_evict_size_based_history() {
        // Pre-populate a full retention window of rotated segments, then
        // simulate many restarts that only *append* to a still-under-
        // threshold active file. Under size-only rotation every pre-
        // existing segment must still exist afterwards — the property
        // the old unconditional rotate-on-start violated (thirteen
        // restarts would have pruned the whole window).
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        let max_files = 5;
        let segment_timestamps: Vec<u64> = (1_000_u64..1_000 + max_files as u64).collect();
        for &ts in &segment_timestamps {
            fs::write(
                boss_log_files::rotated_segment_path(&path, ts),
                format!("history-{ts}\n"),
            )
            .unwrap();
        }
        // Seed an under-threshold active file, then grow it by append
        // across restarts (truncate would not model real restarts).
        fs::write(&path, b"seed\n").unwrap();
        let mut expected_active = String::from("seed\n");
        for i in 0..13 {
            let line = format!("restart-line-{i}\n");
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(line.as_bytes())
                .unwrap();
            expected_active.push_str(&line);
            rotate_on_startup(&path, 100 * 1024 * 1024, max_files);
        }

        assert!(path.exists(), "active file must survive the restart storm");
        let active = fs::read_to_string(&path).unwrap();
        assert_eq!(
            active, expected_active,
            "active file must retain every appended line across restarts"
        );
        let backups = rotated_segments(&path);
        assert_eq!(
            backups.len(),
            max_files,
            "all pre-created rotated segments must still exist (zero eviction)"
        );
        for &ts in &segment_timestamps {
            let segment = boss_log_files::rotated_segment_path(&path, ts);
            assert!(
                segment.exists(),
                "pre-created segment {ts} must not be pruned by under-threshold restarts"
            );
            assert_eq!(fs::read_to_string(&segment).unwrap(), format!("history-{ts}\n"));
        }
    }

    #[test]
    fn restart_storm_with_always_over_threshold_prunes_preexisting_segments() {
        // Negative control: if every restart *does* rotate (max_bytes of
        // 0 / always-over-threshold), the same 13-restart burst consumes
        // rotation slots and prunes pre-existing segments — the behaviour
        // the size gate removed.
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        let max_files = 5;
        let preexisting: Vec<u64> = (1_000_u64..1_000 + max_files as u64).collect();
        for &ts in &preexisting {
            fs::write(
                boss_log_files::rotated_segment_path(&path, ts),
                format!("history-{ts}\n"),
            )
            .unwrap();
        }
        fs::write(&path, b"seed\n").unwrap();
        for i in 0..13 {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(format!("restart-line-{i}\n").as_bytes())
                .unwrap();
            // max_bytes = 1: any non-empty file is over threshold.
            rotate_on_startup(&path, 1, max_files);
        }

        let backups = rotated_segments(&path);
        assert!(
            backups.len() <= max_files,
            "prune must still enforce max_files after always-rotate restarts"
        );
        for &ts in &preexisting {
            let segment = boss_log_files::rotated_segment_path(&path, ts);
            assert!(
                !segment.exists(),
                "pre-existing segment {ts} should be pruned when every restart rotates"
            );
        }
    }

    #[test]
    fn rotate_on_startup_noop_when_file_absent() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        rotate_on_startup(&path, 5, 5);
        assert!(rotated_segments(&path).is_empty());
    }

    #[test]
    fn prune_keeps_n_most_recent() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        // Create 8 fake rotated files with ascending timestamps.
        for i in 1_000_u64..=1_007 {
            fs::write(boss_log_files::rotated_segment_path(&path, i), b"data").unwrap();
        }

        prune_old_rotated(&path, 5);

        let backups = rotated_segments(&path);
        assert_eq!(backups.len(), 5, "expected 5 survivors");
        // The 5 newest (highest timestamp) must survive.
        let mut names: Vec<_> = backups
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for (i, name) in names.iter().enumerate() {
            let expected_ts = 1003 + i as u64;
            assert!(
                name.ends_with(&expected_ts.to_string()),
                "expected ts {expected_ts} in {name}"
            );
        }
    }

    #[test]
    fn prune_noop_when_within_limit() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        for i in 0_u64..3 {
            fs::write(boss_log_files::rotated_segment_path(&path, i), b"data").unwrap();
        }
        prune_old_rotated(&path, 5);
        assert_eq!(rotated_segments(&path).len(), 3);
    }

    #[test]
    fn rotating_writer_rotates_on_threshold() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        let file = open_trace_file(&path).unwrap();
        let state = Arc::new(Mutex::new(Some(RotatingState::new(file))));
        let mut writer = writer(path.clone(), state.clone(), 10, 3);

        // 15 bytes exceeds the 10-byte threshold.
        writer.write_all(b"123456789012345").unwrap();

        let backups = rotated_segments(&path);
        assert_eq!(backups.len(), 1, "expected one rotated backup after write");
        assert!(path.exists(), "new active file should exist after rotation");
        let guard = state.lock().unwrap();
        let s = guard.as_ref().unwrap();
        assert_eq!(s.bytes_written, 0, "byte counter should reset after rotation");
    }

    #[test]
    fn rotating_writer_prunes_beyond_max_files() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        // Pre-populate 3 old rotated files.
        for i in 1_000_u64..=1_002 {
            fs::write(boss_log_files::rotated_segment_path(&path, i), b"old").unwrap();
        }
        let file = open_trace_file(&path).unwrap();
        let state = Arc::new(Mutex::new(Some(RotatingState::new(file))));
        let mut writer = writer(path.clone(), state, 5, 2);

        // 6 bytes exceeds the 5-byte threshold → rotation + prune.
        writer.write_all(b"123456").unwrap();

        let backups = rotated_segments(&path);
        assert!(
            backups.len() <= 2,
            "expected at most 2 rotated backups after prune, got {}",
            backups.len()
        );
    }

    #[test]
    fn no_rotation_below_threshold() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        let file = open_trace_file(&path).unwrap();
        let state = Arc::new(Mutex::new(Some(RotatingState::new(file))));
        let mut writer = writer(path.clone(), state, 100, 3);

        writer.write_all(b"small").unwrap();

        assert!(
            rotated_segments(&path).is_empty(),
            "no rotation expected below threshold"
        );
    }

    #[test]
    fn noop_writer_when_state_is_none() {
        let dir = TempDir::new().unwrap();
        let path = tmp_trace(&dir);
        let state: Arc<Mutex<Option<RotatingState>>> = Arc::new(Mutex::new(None));
        let mut writer = writer(path.clone(), state, 10, 3);
        // Should not panic or create any file.
        let n = writer.write(b"data").unwrap();
        assert_eq!(n, 4);
        assert!(!path.exists());
    }
}
