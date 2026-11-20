//! Shared wall-clock timeout policy for external check execution.
//!
//! Component and declarative checks use the same manifest `limits.timeout_ms`
//! override and proportional default, while retaining separate host ceilings.
//! Declarative subprocesses are supervised synchronously: if the parent cannot
//! observe their completion before the deadline, it kills the child and returns
//! an error rather than treating an incomplete check as clean.

use std::fmt;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::ExternalCheckLimits;

/// Base wall-clock budget for component checks (5 seconds). Used as the fixed
/// component of the proportional timeout formula when no explicit
/// `limits.timeout_ms` override is set in the check manifest.
///
pub(crate) const BASE_COMPONENT_TIMEOUT_MS: u64 = 5_000;
/// Per-file wall-clock budget increment (100 ms per changed file). Combined
/// with [`BASE_COMPONENT_TIMEOUT_MS`] to form a proportional default timeout.
pub(crate) const PER_FILE_COMPONENT_TIMEOUT_MS: u64 = 100;
/// Maximum component timeout a manifest may request (5 minutes). Requests above this are
/// silently clamped so out-of-tree manifests cannot hang the host unboundedly.
pub(crate) const HOST_CEILING_TIMEOUT_MS: u64 = 300_000;
/// Maximum declarative timeout a manifest may request (15 minutes). Bazel-backed
/// declarative checks can legitimately wait on a shared Bazel server longer than
/// the component ceiling, while remaining bounded.
pub(crate) const DECLARATIVE_HOST_CEILING_TIMEOUT_MS: u64 = 900_000;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Bounded time to collect pipe readers after the direct child exits.
///
/// A descendant may keep a pipe open after the child exits, so this cannot be
/// unbounded. It deliberately does not consume the subprocess execution budget:
/// on overrun, return the output captured so far with the child's real status.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Named timeout cause so optional diagnostics can preserve ordinary probe
/// failures without ever swallowing a deadline violation.
#[derive(Debug)]
pub(crate) struct SubprocessTimeout {
    check_id: String,
    subprocess: String,
    timeout_ms: u64,
    elapsed_ms: u128,
}

/// A single wall-clock budget shared by every subprocess in one check execution.
#[derive(Clone, Copy)]
pub(crate) struct CheckDeadline {
    started: Instant,
    budget: Duration,
}

impl CheckDeadline {
    pub(crate) fn new(timeout_ms: u64) -> Self {
        Self {
            started: Instant::now(),
            budget: Duration::from_millis(timeout_ms),
        }
    }

    /// The unconsumed portion of the check-wide budget, in milliseconds.
    pub(crate) fn remaining_ms(self) -> u64 {
        self.budget
            .saturating_sub(self.started.elapsed())
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

impl fmt::Display for SubprocessTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "check `{}` subprocess `{}` exceeded its {} ms wall-clock limit after {} ms",
            self.check_id, self.subprocess, self.timeout_ms, self.elapsed_ms
        )
    }
}

impl std::error::Error for SubprocessTimeout {}

/// Resolve the shared proportional timeout formula for `n_files`, clamped to
/// the caller's host ceiling.
pub(crate) fn resolve_timeout_ms(
    limits: Option<&ExternalCheckLimits>,
    n_files: usize,
    host_ceiling_timeout_ms: u64,
) -> u64 {
    if let Some(explicit) = limits.and_then(|limits| limits.timeout_ms) {
        explicit.min(host_ceiling_timeout_ms)
    } else {
        BASE_COMPONENT_TIMEOUT_MS
            .saturating_add(PER_FILE_COMPONENT_TIMEOUT_MS.saturating_mul(n_files as u64))
            .min(host_ceiling_timeout_ms)
    }
}

/// Run a command to completion under the shared timeout budget.
///
/// stdout and stderr are drained concurrently so a verbose tool cannot block on
/// a full pipe before the timeout supervisor can observe it. This is fail-closed:
/// a timed-out process is killed and reported as an execution error.
pub(crate) fn output_with_timeout(
    command: &mut Command,
    check_id: &str,
    subprocess: &str,
    timeout_ms: u64,
) -> Result<Output> {
    let _home = super::environment::prepare(command)?;
    // Match `Command::output()`: subprocesses must see immediate EOF rather
    // than inheriting checkleft's stdin (which is the pre-push ref list there).
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn subprocess `{subprocess}` for check `{check_id}`"))?;
    let stdout = child.stdout.take().expect("stdout pipe configured before spawn");
    let stderr = child.stderr.take().expect("stderr pipe configured before spawn");
    let stdout_reader = PipeReader::spawn(stdout);
    let stderr_reader = PipeReader::spawn(stderr);
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);

    let status = loop {
        match child.try_wait() {
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err).context("failed to poll subprocess status");
            }
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let elapsed_ms = started.elapsed().as_millis();
                if let Err(err) = child.kill() {
                    return Err(err).with_context(|| {
                        format!(
                            "check `{check_id}` subprocess `{subprocess}` exceeded its {timeout_ms} ms wall-clock limit after {elapsed_ms} ms and could not be terminated"
                        )
                    });
                }
                let _ = child.wait();
                return Err(SubprocessTimeout {
                    check_id: check_id.to_owned(),
                    subprocess: subprocess.to_owned(),
                    timeout_ms,
                    elapsed_ms,
                }
                .into());
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
        }
    };

    // Once the child has exited, give the readers a fixed grace (not the
    // residual execution budget) to reach EOF. A descendant that inherited the
    // pipe may hold it open indefinitely; that is not an execution timeout. The
    // child's real status and the bytes captured so far are returned instead.
    let child_elapsed_ms = started.elapsed().as_millis();
    let drain_deadline = Instant::now() + DRAIN_GRACE;
    let stdout = stdout_reader.finish(
        drain_deadline,
        "stdout",
        check_id,
        subprocess,
        &status,
        child_elapsed_ms,
    )?;
    let stderr = stderr_reader.finish(
        drain_deadline,
        "stderr",
        check_id,
        subprocess,
        &status,
        child_elapsed_ms,
    )?;
    Ok(Output { status, stdout, stderr })
}

/// Reads a pipe on a background thread, accumulating bytes incrementally so a
/// pipe that never reaches EOF still yields everything read so far.
struct PipeReader {
    buffer: Arc<Mutex<Vec<u8>>>,
    done: Receiver<Result<()>>,
}

impl PipeReader {
    fn spawn(mut pipe: impl Read + Send + 'static) -> Self {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let (tx, done) = mpsc::sync_channel(1);
        let sink = Arc::clone(&buffer);
        thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            let result = loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break Ok(()),
                    Ok(n) => sink
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .extend_from_slice(&chunk[..n]),
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(err) => break Err(anyhow::Error::new(err).context("failed to read subprocess output")),
                }
            };
            let _ = tx.send(result);
        });
        Self { buffer, done }
    }

    fn finish(
        self,
        deadline: Instant,
        stream: &str,
        check_id: &str,
        subprocess: &str,
        status: &std::process::ExitStatus,
        child_elapsed_ms: u128,
    ) -> Result<Vec<u8>> {
        let wait = deadline.saturating_duration_since(Instant::now());
        match self.done.recv_timeout(wait) {
            Ok(result) => result
                .with_context(|| format!("failed to read {stream} for check `{check_id}` subprocess `{subprocess}`"))?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    check_id,
                    subprocess,
                    stream,
                    status = %status,
                    child_elapsed_ms,
                    drain_grace_ms = DRAIN_GRACE.as_millis() as u64,
                    "descendant held the output pipe open after child exit; using output captured so far"
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("{stream} reader stopped for check `{check_id}` subprocess `{subprocess}`")
            }
        }
        Ok(std::mem::take(
            &mut *self.buffer.lock().unwrap_or_else(|e| e.into_inner()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn exited_child_with_pipe_held_open_is_not_a_timeout() {
        let started = Instant::now();
        let out = output_with_timeout(
            &mut sh("echo partial; echo err >&2; (sleep 6 &) ; exit 3"),
            "t/c",
            "tool",
            60_000,
        )
        .expect("drain overrun must not be an error");
        assert!(started.elapsed() < Duration::from_millis(3_500));
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"partial\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[test]
    fn exited_zero_with_pipe_held_open_succeeds() {
        let started = Instant::now();
        let out = output_with_timeout(&mut sh("echo ok; (sleep 6 &)"), "t/c", "tool", 60_000).unwrap();
        assert!(started.elapsed() < Duration::from_millis(3_500));
        assert!(out.status.success());
        assert_eq!(out.stdout, b"ok\n");
    }

    #[test]
    fn only_stdout_held_open_preserves_both_streams() {
        let started = Instant::now();
        let out = output_with_timeout(
            &mut sh("echo ok; echo err >&2; (sleep 6 2>/dev/null &)"),
            "t/c",
            "tool",
            60_000,
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(out.status.success());
        assert_eq!(out.stdout, b"ok\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[test]
    fn genuinely_slow_child_still_times_out() {
        let err = output_with_timeout(&mut sh("exec sleep 6"), "t/c", "tool", 200).unwrap_err();
        assert!(err.downcast_ref::<SubprocessTimeout>().is_some());
    }
}
