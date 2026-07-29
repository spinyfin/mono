//! Shared, safe `.jsonl` line-append utility.
//!
//! A JSONL writer that issues a record's body and its trailing newline as
//! two separate `write()` calls is corruptible under concurrency: `O_APPEND`
//! makes each individual `write()` atomic, but not the *pair* of them, so
//! two interleaved appenders can produce `bodyAbodyB\n\n` on disk instead of
//! two well-formed lines. This is exactly the defect that motivated this
//! crate — see the dispatch-events sink, `boss-dispatch-events`, which used
//! to reimplement JSONL appending inline with that two-write bug.
//!
//! [`JsonlAppender`] fixes this by serializing every append — across all
//! paths — through one process-wide `tokio::sync::Mutex`. That is a
//! correctness-first choice over "build one buffer, issue one `write_all`":
//! a single `write_all` call is atomic against concurrent appenders in
//! practice (a single `write()` to a regular file opened with `O_APPEND`
//! essentially never returns short), but `write_all` is not *documented* to
//! issue exactly one syscall for arbitrarily large input, so relying on it
//! alone leaves a correctness guarantee that quietly depends on record
//! size. Serializing through a mutex removes that dependency entirely: no
//! two appends' writes can ever be in flight at the same time, regardless
//! of how many syscalls either one takes.
//!
//! File handles are deliberately NOT cached across calls. Some callers
//! append to an effectively unbounded set of paths over a long-lived
//! process's lifetime (e.g. one per-execution mirror file per dispatched
//! execution) — caching a handle per path would leak file descriptors.
//! Each [`JsonlAppender::append`] call opens, writes, and closes.
//!
//! This utility serializes writers **within one process**. It does not
//! provide cross-process exclusion (that needs a real file lock, e.g.
//! `flock`) — `tools/boss/event-shim` already does that correctly for its
//! own multi-process use case and is not a caller of this crate.

use std::io;
use std::path::Path;

use serde::Serialize;
use tokio::sync::Mutex;

/// Serializes concurrent `.jsonl` appends within this process so that no
/// two records' bytes can ever interleave on disk.
#[derive(Debug, Default)]
pub struct JsonlAppender {
    lock: Mutex<()>,
}

impl JsonlAppender {
    pub fn new() -> Self {
        Self::default()
    }

    /// Serialize `value` to a JSON line and append it (with a trailing
    /// newline) to `path` as one write, creating `path`'s parent directory
    /// and the file itself if either is missing.
    pub async fn append(&self, path: &Path, value: &impl Serialize) -> io::Result<()> {
        let mut line = serde_json::to_vec(value).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        line.push(b'\n');

        let _guard = self.lock.lock().await;
        append_locked(path, &line)
    }
}

fn append_locked(path: &Path, line: &[u8]) -> io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use serde::Deserialize;
    use tempfile::TempDir;

    #[derive(Debug, Serialize, Deserialize)]
    struct Record {
        writer: usize,
        seq: usize,
        // Padding so records are close to the ~270-560 byte range the
        // motivating dispatch-events corruption was observed at.
        padding: String,
    }

    #[tokio::test]
    async fn appends_body_and_newline_as_one_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("out.jsonl");
        let appender = JsonlAppender::new();

        appender
            .append(
                &path,
                &Record {
                    writer: 0,
                    seq: 0,
                    padding: "x".repeat(16),
                },
            )
            .await
            .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
        let parsed: Record = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(parsed.seq, 0);
    }

    #[tokio::test]
    async fn creates_missing_parent_directory() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("dir").join("out.jsonl");
        let appender = JsonlAppender::new();

        appender
            .append(
                &path,
                &Record {
                    writer: 0,
                    seq: 0,
                    padding: String::new(),
                },
            )
            .await
            .unwrap();

        assert!(path.exists());
    }

    /// Regression test for the dispatch-events corruption: many concurrent
    /// tasks appending through ONE shared `JsonlAppender` must never
    /// produce a line that fails to parse as exactly one JSON record, and
    /// every emitted record must be present exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_appends_never_interleave() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("current.jsonl");
        let appender = Arc::new(JsonlAppender::new());

        const WRITERS: usize = 40;
        const PER_WRITER: usize = 50;

        let mut handles = Vec::new();
        for writer in 0..WRITERS {
            let appender = Arc::clone(&appender);
            let path = path.clone();
            handles.push(tokio::spawn(async move {
                for seq in 0..PER_WRITER {
                    appender
                        .append(
                            &path,
                            &Record {
                                writer,
                                seq,
                                // Vary length across writers, echoing the
                                // ~270-560 byte real-world record sizes.
                                padding: "y".repeat(200 + (writer * 7) % 300),
                            },
                        )
                        .await
                        .unwrap();
                }
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), WRITERS * PER_WRITER, "no line may be lost or merged");

        let mut seen = std::collections::HashSet::new();
        for line in lines {
            let record: Record = serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("line failed to parse as exactly one record: {err}\nline: {line}"));
            assert!(
                seen.insert((record.writer, record.seq)),
                "duplicate record observed: {record:?}"
            );
        }
        assert_eq!(seen.len(), WRITERS * PER_WRITER);
    }
}
