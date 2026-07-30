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
//! paths, and across every [`JsonlAppender`] instance, since the lock is a
//! crate-level `static` rather than an instance field — through one
//! process-wide `tokio::sync::Mutex`. That is a correctness-first choice
//! over "build one buffer, issue one `write_all`": a single `write_all`
//! call is atomic against concurrent appenders in practice (a single
//! `write()` to a regular file opened with `O_APPEND` essentially never
//! returns short), but `write_all` is not *documented* to issue exactly one
//! syscall for arbitrarily large input, so relying on it alone leaves a
//! correctness guarantee that quietly depends on record size. Serializing
//! through a mutex removes that dependency entirely: no two appends'
//! writes can ever be in flight at the same time, regardless of how many
//! syscalls either one takes, and regardless of how many `JsonlAppender`
//! values a caller happens to construct.
//!
//! File handles are deliberately NOT cached across calls. Some callers
//! append to an effectively unbounded set of paths over a long-lived
//! process's lifetime (e.g. one per-execution mirror file per dispatched
//! execution) — caching a handle per path would leak file descriptors.
//! Each [`JsonlAppender::append`] call opens, writes, and closes.
//!
//! Serializing through a process-wide mutex is not sufficient on its own:
//! `<state-root>/dispatch-events/current.jsonl` has more than one potential
//! writer *process* (an app-autostarted engine plus a hand-started one, or an
//! engine outliving a crash-recovery relaunch), and a mutex in one address
//! space says nothing about the other. So each append also takes an
//! **exclusive advisory lock on the target file** for the duration of the
//! write, via `fs4` — the same `flock` mechanism `tools/boss/event-shim`,
//! `tools/cube`, and `tools/repobin` already use for their multi-process
//! buffers. Both layers are kept: the mutex is what keeps the *set* of paths
//! in one `append_to_all` call from interleaving with another task's set, and
//! the file lock is what makes each individual append atomic against other
//! processes.
//!
//! The lock is advisory, so it only excludes writers that also take it. Every
//! writer of these files goes through this crate; anything that appends to a
//! Boss `.jsonl` by hand is outside the guarantee and must not.
//!
//! **The file lock is bounded, and never an availability dependency.** The
//! obvious implementation — a blocking `lock_exclusive` — would make the
//! forensic writer able to stall the thing it is recording: because the
//! process-wide mutex is held across the whole path set, one wedged holder of
//! one file's advisory lock (a crashed-but-not-reaped process, a debugging
//! `flock(1)` on `current.jsonl`, a lock on a network-mounted state root)
//! would block *every* append in this process — including appends to unrelated
//! per-execution mirrors — for as long as it kept the lock. Dispatch events are
//! output *about* the dispatch path and must not be able to stop it. So
//! acquisition is [`LOCK_ACQUIRE_TIMEOUT`]-bounded `try_lock_exclusive` with
//! backoff, and on timeout the append takes a defined, logged branch: it
//! proceeds with the single `write_all` anyway and warns on stderr.
//!
//! Falling back to the unlocked write (rather than returning an error) is the
//! deliberate choice, and it is the same trade the two-layer design makes
//! above: a single `write_all` of a record this size is atomic against
//! concurrent appenders in practice, so the fallback's exposure is the
//! *theoretical* short-write case, whereas dropping the append loses a
//! forensic record for certain. This crate exists because records were being
//! lost; it must not itself become a way to lose them. The warning is what
//! makes the degraded write visible — a wedged lock holder on a Boss state
//! file is an operator problem, not something to absorb silently.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt;
use serde::Serialize;
use tokio::sync::Mutex;

/// Process-wide lock backing every [`JsonlAppender`]. Living at crate scope
/// (rather than as an instance field) is what makes the "one process-wide
/// mutex" guarantee true regardless of how many `JsonlAppender` values a
/// caller constructs against the same or different roots: two independently
/// constructed appenders still serialize through this one lock.
static APPEND_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Longest an append will wait for another *process* to release the target
/// file's advisory lock before writing without it.
///
/// Sized against what the lock actually protects: one `write_all` of a
/// few-hundred-byte record, i.e. microseconds. Half a second is therefore
/// thousands of times the honest contention case — a wait that reaches this
/// bound is a wedged holder, not a busy one, and waiting longer would only
/// deepen the stall it represents.
pub const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_millis(500);

/// First sleep between `try_lock_exclusive` attempts; doubles up to
/// [`LOCK_RETRY_BACKOFF_MAX`]. Starting this short keeps the common case
/// (genuine, microsecond-scale contention with another engine) from paying a
/// scheduler-quantum penalty.
const LOCK_RETRY_BACKOFF_START: Duration = Duration::from_micros(200);

/// Cap on the retry sleep, so a long wait still polls often enough to pick the
/// lock up promptly once it is released.
const LOCK_RETRY_BACKOFF_MAX: Duration = Duration::from_millis(20);

/// Serializes concurrent `.jsonl` appends within this process — across all
/// `JsonlAppender` instances — so that no two records' bytes can ever
/// interleave on disk.
#[derive(Debug, Default)]
pub struct JsonlAppender {
    _private: (),
}

impl JsonlAppender {
    pub fn new() -> Self {
        Self::default()
    }

    /// Serialize `value` to a JSON line and append it (with a trailing
    /// newline) to `path` as one write, creating `path`'s parent directory
    /// and the file itself if either is missing.
    pub async fn append(&self, path: &Path, value: &impl Serialize) -> io::Result<()> {
        self.append_to_all(&[path], value).await.into_iter().next().unwrap()
    }

    /// Serialize `value` to a JSON line once and append it (with a trailing
    /// newline) to every path in `paths`, all under a single acquisition of
    /// the process-wide lock. Returns one result per input path, in order,
    /// so a failure writing one path doesn't stop attempts at the others.
    pub async fn append_to_all(&self, paths: &[&Path], value: &impl Serialize) -> Vec<io::Result<()>> {
        let mut line = match serde_json::to_vec(value) {
            Ok(bytes) => bytes,
            Err(err) => {
                let err_msg = err.to_string();
                return paths
                    .iter()
                    .map(|_| Err(io::Error::new(io::ErrorKind::InvalidData, err_msg.clone())))
                    .collect();
            }
        };
        line.push(b'\n');

        let owned_paths: Vec<PathBuf> = paths.iter().map(|path| path.to_path_buf()).collect();
        let guard = APPEND_LOCK.lock().await;
        let results = tokio::task::spawn_blocking(move || {
            let results = owned_paths.iter().map(|path| append_locked(path, &line)).collect();
            drop(guard);
            results
        })
        .await;
        match results {
            Ok(results) => results,
            Err(join_err) => paths
                .iter()
                .map(|_| Err(io::Error::other(join_err.to_string())))
                .collect(),
        }
    }
}

fn append_locked(path: &Path, line: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let mut file = match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::OpenOptions::new().create(true).append(true).open(path)?
        }
        Err(err) => return Err(err),
    };
    // Exclusive for the duration of the write, so a second *process* appending
    // to the same file cannot land bytes in the middle of this record. Bounded,
    // never blocking indefinitely — see the module docs for why a forensic
    // writer must not be able to stall on a wedged lock holder.
    let locked = acquire_bounded(&file, path)?;
    // Unlock explicitly rather than relying on the handle's close: the write
    // result is the one thing that must survive, so it is captured first and the
    // unlock's own (uninteresting) failure never masks it.
    let written = file.write_all(line);
    if locked {
        let _ = FileExt::unlock(&file);
    }
    written
}

/// Take `file`'s exclusive advisory lock, giving up after
/// [`LOCK_ACQUIRE_TIMEOUT`].
///
/// Returns `Ok(true)` when the lock is held (the caller must unlock),
/// `Ok(false)` when the timeout expired and the caller should write without it
/// — the documented degraded branch, warned about on stderr rather than
/// absorbed. `Err` is reserved for a lock attempt that failed for a reason
/// other than contention, which is a real filesystem problem and belongs in the
/// caller's per-path error handling.
fn acquire_bounded(file: &std::fs::File, path: &Path) -> io::Result<bool> {
    let deadline = Instant::now() + LOCK_ACQUIRE_TIMEOUT;
    let mut backoff = LOCK_RETRY_BACKOFF_START;
    loop {
        match FileExt::try_lock_exclusive(file) {
            Ok(true) => return Ok(true),
            // Contended. `fs4` reports this as `Ok(false)` on some platforms and
            // as `WouldBlock`/`Other` on others, so both shapes retry.
            Ok(false) => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(err),
        }
        if Instant::now() >= deadline {
            eprintln!(
                "warning: {} advisory lock held elsewhere for >{}ms; appending without it. \
                 Another process is wedged holding this file's lock — find and clear it \
                 (`lsof {}`); until then cross-process appends to this file are unprotected.",
                path.display(),
                LOCK_ACQUIRE_TIMEOUT.as_millis(),
                path.display(),
            );
            return Ok(false);
        }
        std::thread::sleep(backoff.min(deadline.saturating_duration_since(Instant::now())));
        backoff = (backoff * 2).min(LOCK_RETRY_BACKOFF_MAX);
    }
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

    /// The process-wide mutex cannot exclude another *process*, and
    /// `dispatch-events/current.jsonl` genuinely has more than one potential
    /// writer process. This asserts the second layer is real: while a foreign
    /// holder owns the file's advisory lock, an append must block rather than
    /// write, and must complete once the lock is released.
    ///
    /// `flock` is per open-file-description, not per process, so a second
    /// `File` opened here contends exactly as another process's handle would —
    /// which is what makes this testable in-process at all.
    ///
    /// The wait asserted here is bounded by [`LOCK_ACQUIRE_TIMEOUT`]; the
    /// 150 ms probe below is well inside it, so this test says "the lock is
    /// honoured", not "the wait is unbounded". The bound itself is pinned by
    /// `append_gives_up_on_a_wedged_holder_and_still_records_the_event`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn append_waits_for_a_foreign_holder_of_the_file_lock() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("current.jsonl");
        std::fs::write(&path, b"").unwrap();

        let foreign = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        FileExt::lock_exclusive(&foreign).unwrap();

        let appender = JsonlAppender::new();
        let append_path = path.clone();
        let appending = tokio::spawn(async move {
            let appender = appender;
            appender
                .append(
                    &append_path,
                    &Record {
                        writer: 1,
                        seq: 1,
                        padding: "z".repeat(256),
                    },
                )
                .await
        });

        // Give the append every chance to run to completion if the lock were
        // not being honoured: it is on a multi-thread runtime with a free
        // worker, so the only thing that can hold it up is the `flock`.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            !appending.is_finished(),
            "append completed while a foreign process held the file lock — cross-process appends can interleave"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "",
            "no bytes may land while the lock is held elsewhere"
        );

        FileExt::unlock(&foreign).unwrap();
        appending.await.unwrap().unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1, "the append must land once unblocked");
        let parsed: Record = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(parsed.seq, 1);
    }

    /// The bound on the wait above. A holder that never releases the lock — a
    /// wedged process, a `flock(1)` left running, a stale lock on a network
    /// state root — must not turn the forensic writer into an availability
    /// dependency for the dispatch path it is recording. After
    /// [`LOCK_ACQUIRE_TIMEOUT`] the append takes the documented degraded
    /// branch: it writes anyway (so the event is not lost) and warns.
    ///
    /// The generous upper bound on the assertion is deliberate: what matters is
    /// that the wait is *bounded at all*, not that it lands on a precise
    /// deadline on a loaded CI host.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn append_gives_up_on_a_wedged_holder_and_still_records_the_event() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("current.jsonl");
        std::fs::write(&path, b"").unwrap();

        // Never unlocked: this stands in for a wedged holder.
        let wedged = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        FileExt::lock_exclusive(&wedged).unwrap();

        let appender = JsonlAppender::new();
        let result = tokio::time::timeout(
            LOCK_ACQUIRE_TIMEOUT * 10,
            appender.append(
                &path,
                &Record {
                    writer: 7,
                    seq: 7,
                    padding: "w".repeat(256),
                },
            ),
        )
        .await
        .expect("append must not hang indefinitely on a wedged lock holder");
        result.expect("the degraded branch writes rather than failing");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents.lines().count(),
            1,
            "the record must still reach the file — losing forensic output is the failure this crate exists to prevent"
        );
        let parsed: Record = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(parsed.seq, 7);

        // A second append must not be permanently poisoned either: the bound is
        // per-attempt, not a latch.
        appender
            .append(
                &path,
                &Record {
                    writer: 8,
                    seq: 8,
                    padding: String::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
    }

    /// A wedged holder of ONE file must not block appends to a DIFFERENT file.
    /// The process-wide mutex means both go through the same critical section,
    /// so an unbounded wait on the first would stall the second — which is the
    /// specific way a forensic writer becomes an availability dependency for
    /// unrelated per-execution mirrors.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_wedged_holder_on_one_file_does_not_stall_appends_to_another() {
        let dir = TempDir::new().unwrap();
        let wedged_path = dir.path().join("current.jsonl");
        let other_path = dir.path().join("executions").join("exec-1").join("dispatch.jsonl");
        std::fs::write(&wedged_path, b"").unwrap();

        let wedged = std::fs::OpenOptions::new().append(true).open(&wedged_path).unwrap();
        FileExt::lock_exclusive(&wedged).unwrap();

        let appender = Arc::new(JsonlAppender::new());
        let blocked = {
            let appender = Arc::clone(&appender);
            let wedged_path = wedged_path.clone();
            tokio::spawn(async move {
                appender
                    .append(
                        &wedged_path,
                        &Record {
                            writer: 1,
                            seq: 1,
                            padding: String::new(),
                        },
                    )
                    .await
            })
        };

        tokio::time::timeout(
            LOCK_ACQUIRE_TIMEOUT * 10,
            appender.append(
                &other_path,
                &Record {
                    writer: 2,
                    seq: 2,
                    padding: String::new(),
                },
            ),
        )
        .await
        .expect("an unrelated mirror must not be held hostage by another file's lock")
        .unwrap();

        blocked.await.unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&other_path).unwrap().lines().count(), 1);
    }
}
