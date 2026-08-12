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
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

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
/// the child has already completed successfully at this point.
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
    let (stdout_tx, stdout_rx) = mpsc::sync_channel(1);
    let (stderr_tx, stderr_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = stdout_tx.send(read_all(stdout));
    });
    thread::spawn(move || {
        let _ = stderr_tx.send(read_all(stderr));
    });
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

    // Once the child has exited its pipe ends are closed. Drain readers under a
    // fixed grace, rather than the residual execution budget, so a successful
    // near-deadline child cannot be reported as a timeout while posting output.
    let stdout = receive_output(
        stdout_rx,
        "stdout",
        started,
        DRAIN_GRACE,
        check_id,
        subprocess,
        timeout_ms,
    )?;
    let stderr = receive_output(
        stderr_rx,
        "stderr",
        started,
        DRAIN_GRACE,
        check_id,
        subprocess,
        timeout_ms,
    )?;
    Ok(Output { status, stdout, stderr })
}

fn receive_output(
    receiver: Receiver<Result<Vec<u8>>>,
    stream: &str,
    started: Instant,
    wait: Duration,
    check_id: &str,
    subprocess: &str,
    timeout_ms: u64,
) -> Result<Vec<u8>> {
    match receiver.recv_timeout(wait) {
        Ok(output) => {
            output.with_context(|| format!("failed to read {stream} for check `{check_id}` subprocess `{subprocess}`"))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => Err(SubprocessTimeout {
            check_id: check_id.to_owned(),
            subprocess: subprocess.to_owned(),
            timeout_ms,
            elapsed_ms: started.elapsed().as_millis(),
        }
        .into()),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!(
            "{stream} reader stopped for check `{check_id}` subprocess `{subprocess}`"
        )),
    }
}

fn read_all(mut pipe: impl Read) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    pipe.read_to_end(&mut output)
        .context("failed to read subprocess output")?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    /// Residual execution budget exhausted must not fail the post-exit drain.
    ///
    /// Production code drains under fixed [`DRAIN_GRACE`], not the leftover
    /// subprocess budget, so a child that finished near the deadline is not
    /// reported as a timeout while its pipe readers deliver. This asserts that
    /// path without wall-clock sleeps or load sensitivity.
    #[test]
    fn receive_output_ok_when_residual_budget_exhausted() {
        let timeout_ms = 100;
        let started = Instant::now()
            .checked_sub(Duration::from_millis(timeout_ms) + Duration::from_secs(1))
            .expect("Instant can subtract past the execution budget");
        assert!(
            started.elapsed() >= Duration::from_millis(timeout_ms),
            "test setup must start with residual budget already zero"
        );

        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = tx.send(Ok(b"drained\n".to_vec()));
        });

        let output = receive_output(
            rx,
            "stdout",
            started,
            DRAIN_GRACE,
            "test/drain-grace",
            "tool",
            timeout_ms,
        )
        .expect("residual budget exhausted must not fail the drain");
        assert_eq!(output, b"drained\n");
    }
}
