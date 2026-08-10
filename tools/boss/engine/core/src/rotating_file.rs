//! Bounded append-only file writer shared by engine log surfaces.
//!
//! Rotated segments use `boss-log-files`' collision-safe timestamp naming and
//! are pruned after every rotation, including rotations caused by a restart.
//! Record-preserving writers rotate before a record that would not fit, so a
//! segment can exceed its configured limit by one record rather than splitting
//! that record across files.
//!
//! ## Text log configuration
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `BOSS_ENGINE_LOG_MAX_BYTES` | `104857600` (100 MiB) | Rotate before a log record would exceed this size |
//! | `BOSS_ENGINE_LOG_MAX_FILES` | `20` | Keep at most this many rotated text-log backups |

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use boss_log_files::{next_rotated_path, rotated_segments};

/// Open (or create) `path` for appending, creating its parent if needed.
pub fn open_append_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Delete the oldest rotated backups, retaining at most `max_files`.
pub fn prune_old_rotated(active_path: &Path, max_files: usize, log_name: &str) {
    let backups = rotated_segments(active_path);
    let to_delete = backups.len().saturating_sub(max_files);
    for path in &backups[..to_delete] {
        if let Err(err) = std::fs::remove_file(path) {
            eprintln!("boss-engine: could not prune old {log_name} {}: {err}", path.display());
        }
    }
}

/// Mutable state shared by all instances of a [`RotatingFileWriter`].
pub struct RotatingState {
    pub file: File,
    pub bytes_written: u64,
}

impl RotatingState {
    pub fn new(file: File) -> Self {
        let bytes_written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        Self { file, bytes_written }
    }
}

/// A bounded append-only writer that rotates before a segment can exceed its
/// configured limit. If rotation fails, it drops file output rather than
/// continuing to grow the active file without bound; stderr remains live.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct RotatingFileWriter {
    pub path: PathBuf,
    pub state: Arc<Mutex<Option<RotatingState>>>,
    pub max_bytes: u64,
    pub max_files: usize,
    pub log_name: &'static str,
    /// When true, write a complete record and then rotate if it crosses the
    /// limit. When false, rotate before a record that would cross the limit.
    pub rotate_after_write: bool,
}

impl RotatingFileWriter {
    fn rotate(state: &mut RotatingState, path: &Path, max_files: usize, log_name: &str) -> bool {
        let rotated = next_rotated_path(path);
        if let Err(err) = std::fs::rename(path, &rotated) {
            eprintln!("boss-engine: could not rotate {log_name}: {err}");
            return false;
        }
        match open_append_file(path) {
            Ok(file) => {
                state.file = file;
                state.bytes_written = 0;
                prune_old_rotated(path, max_files, log_name);
                true
            }
            Err(err) => {
                eprintln!("boss-engine: could not open new {log_name} after rotation: {err}");
                false
            }
        }
    }
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let original_len = buf.len();
        let max_bytes = self.max_bytes.max(1);
        let Ok(mut guard) = self.state.lock() else {
            return Ok(original_len);
        };
        let Some(state) = guard.as_mut() else {
            return Ok(original_len);
        };

        if self.rotate_after_write {
            if state.file.write_all(buf).is_ok() {
                state.bytes_written = state.bytes_written.saturating_add(buf.len() as u64);
                if state.bytes_written >= max_bytes {
                    let _ = Self::rotate(state, &self.path, self.max_files, self.log_name);
                }
            }
        } else {
            let record_would_cross_limit =
                state.bytes_written > 0 && buf.len() as u64 > max_bytes.saturating_sub(state.bytes_written);
            if (state.bytes_written >= max_bytes || record_would_cross_limit)
                && !Self::rotate(state, &self.path, self.max_files, self.log_name)
            {
                return Ok(original_len);
            }
            if state.file.write_all(buf).is_ok() {
                state.bytes_written = state.bytes_written.saturating_add(buf.len() as u64);
            }
        }
        Ok(original_len)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Ok(mut guard) = self.state.lock()
            && let Some(state) = guard.as_mut()
        {
            let _ = state.file.flush();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn writer_keeps_records_intact_and_prunes_after_rotation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine.log");
        let state = Arc::new(Mutex::new(Some(RotatingState::new(open_append_file(&path).unwrap()))));
        let mut writer = RotatingFileWriter {
            path: path.clone(),
            state,
            max_bytes: 5,
            max_files: 2,
            log_name: "test log",
            rotate_after_write: false,
        };

        writer.write_all(b"first").unwrap();
        writer.write_all(b"second").unwrap();
        writer.write_all(b"third").unwrap();
        writer.write_all(b"fourth").unwrap();

        let segments = rotated_segments(&path);
        assert_eq!(segments.len(), 2, "retention must prune after repeated rotations");
        assert_eq!(fs::read_to_string(&path).unwrap(), "fourth");
        let contents: Vec<_> = segments
            .iter()
            .map(|segment| fs::read_to_string(segment).unwrap())
            .collect();
        assert!(
            contents
                .iter()
                .all(|record| ["second", "third"].contains(&record.as_str()))
        );
    }

    #[test]
    fn writer_continues_logging_after_rotation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine.log");
        let state = Arc::new(Mutex::new(Some(RotatingState::new(open_append_file(&path).unwrap()))));
        let mut writer = RotatingFileWriter {
            path: path.clone(),
            state,
            max_bytes: 5,
            max_files: 2,
            log_name: "test log",
            rotate_after_write: false,
        };

        writer.write_all(b"first").unwrap();
        writer.write_all(b"next").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "next");
    }

    #[test]
    fn writer_clamps_zero_max_bytes_without_spinning() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine.log");
        let state = Arc::new(Mutex::new(Some(RotatingState::new(open_append_file(&path).unwrap()))));
        let mut writer = RotatingFileWriter {
            path: path.clone(),
            state,
            max_bytes: 0,
            max_files: 2,
            log_name: "test log",
            rotate_after_write: false,
        };

        writer.write_all(b"record").unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "record");
    }

    #[test]
    fn prune_enforces_retention_after_a_restart_storm() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("engine.log");
        for timestamp in 1_000..1_005 {
            fs::write(boss_log_files::rotated_segment_path(&path, timestamp), b"old").unwrap();
        }

        // Startup prunes independently of a new write, so repeated starts
        // cannot accumulate one retained segment per restart.
        prune_old_rotated(&path, 2, "test log");

        assert_eq!(rotated_segments(&path).len(), 2);
    }
}
