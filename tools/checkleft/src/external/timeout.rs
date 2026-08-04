//! Shared wall-clock timeout policy for external check execution.
//!
//! Component and declarative checks use the same manifest `limits.timeout_ms`
//! override, proportional default, and host ceiling. Declarative subprocesses
//! are supervised synchronously: if the parent cannot observe their completion
//! before the deadline, it kills the child and returns an error rather than
//! treating an incomplete check as clean.

use std::fmt;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::ExternalCheckLimits;

/// Base wall-clock budget for external checks (15 seconds). Used as the fixed
/// component of the proportional timeout formula when no explicit
/// `limits.timeout_ms` override is set in the check manifest.
///
/// A subprocess can be queued briefly on a loaded CI host even when it is not
/// wedged. Fifteen seconds leaves room for that normal scheduling variance while
/// still failing a stalled check promptly.
pub(crate) const BASE_COMPONENT_TIMEOUT_MS: u64 = 15_000;
/// Per-file wall-clock budget increment (100 ms per changed file). Combined
/// with [`BASE_COMPONENT_TIMEOUT_MS`] to form a proportional default timeout.
pub(crate) const PER_FILE_COMPONENT_TIMEOUT_MS: u64 = 100;
/// Maximum timeout a manifest may request (15 minutes). Requests above this are
/// silently clamped so out-of-tree manifests cannot hang the host unboundedly.
pub(crate) const HOST_CEILING_TIMEOUT_MS: u64 = 900_000;

const POLL_INTERVAL: Duration = Duration::from_millis(10);

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

/// Resolve the shared component/declarative timeout budget for `n_files`.
pub(crate) fn resolve_timeout_ms(limits: Option<&ExternalCheckLimits>, n_files: usize) -> u64 {
    if let Some(explicit) = limits.and_then(|limits| limits.timeout_ms) {
        explicit.min(HOST_CEILING_TIMEOUT_MS)
    } else {
        BASE_COMPONENT_TIMEOUT_MS
            .saturating_add(PER_FILE_COMPONENT_TIMEOUT_MS.saturating_mul(n_files as u64))
            .min(HOST_CEILING_TIMEOUT_MS)
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

    // Once the child has exited both pipes are closed, so allow reader threads
    // a short grace period instead of reusing an already-exhausted deadline.
    let reader_grace = timeout.saturating_sub(started.elapsed()).max(Duration::from_secs(1));
    let stdout = receive_output(
        stdout_rx,
        "stdout",
        started,
        reader_grace,
        check_id,
        subprocess,
        timeout_ms,
    )?;
    let stderr = receive_output(
        stderr_rx,
        "stderr",
        started,
        reader_grace,
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
