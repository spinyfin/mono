use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(test)]
use std::thread;
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

use crate::app::CubeError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    pub cwd: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    /// Extra environment variables to inject into the child process.
    /// Applied after cube's standard env (NO_COLOR, GIT_SSH_COMMAND, etc.).
    pub env: Vec<(String, String)>,
}

pub trait CommandRunner {
    /// Shared-store critical section for PR bookmark writes and GC.
    fn lock_pr_bookmarks(&self, cwd: &Path) -> Result<crate::lock::RepoLock, CubeError> {
        crate::lock::RepoLock::for_workspace(cwd)
    }

    fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError>;

    /// Cleanup runs in an isolated group and retains both output streams.
    fn run_cleanup(&self, invocation: &CommandInvocation, timeout: Duration) -> Result<String, CubeError> {
        self.run_with_timeout(invocation, timeout)
    }

    /// Run a command with an upper bound on its wall-clock time. On expiry the
    /// child process is killed and reaped, and [`CubeError::CommandTimedOut`]
    /// is returned, so cube never blocks indefinitely on a wedged subprocess
    /// (e.g. a `jj git fetch` whose ssh connection went half-open).
    ///
    /// The default implementation ignores the timeout and delegates to
    /// [`CommandRunner::run`]; test fakes complete instantly, so they need no
    /// timeout handling. [`RealCommandRunner`] overrides this to enforce the
    /// bound against a real child process.
    fn run_with_timeout(&self, invocation: &CommandInvocation, _timeout: Duration) -> Result<String, CubeError> {
        self.run(invocation)
    }
}

/// How often [`RealCommandRunner::run_with_timeout`] emits a stderr progress
/// line while a subprocess is still running. Long enough not to spam a
/// normal-latency command, short enough that a caller's own output-silence
/// timeout (commonly ~120s) never fires on a command that is still making
/// progress under lock contention.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

pub struct RealCommandRunner;

impl RealCommandRunner {
    pub fn invocation(cwd: &Path, program: &str, args: &[&str]) -> CommandInvocation {
        CommandInvocation {
            cwd: cwd.to_path_buf(),
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            env: vec![],
        }
    }

    /// Apply the environment cube wants on every subprocess it spawns.
    fn configure_env(cmd: &mut Command) {
        // When cube's own stdout is not a terminal (e.g. piped by worker
        // automation), tell subprocesses to suppress ANSI colour codes and
        // interactive chrome. NO_COLOR is the cross-ecosystem standard; both
        // jj and gh honour it.
        if !std::io::stdout().is_terminal() {
            cmd.env("NO_COLOR", "1");
        }

        // Make a dead ssh connection fail in seconds instead of hanging on a
        // half-open TCP socket — the failure mode that wedged the per-repo
        // lock for 16+ minutes. `git` honours GIT_SSH_COMMAND directly; for
        // jj's own transport this is best-effort, with the run_with_timeout
        // bound as the hard backstop. Respect an operator-supplied override.
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            cmd.env(
                "GIT_SSH_COMMAND",
                "ssh -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=3",
            );
        }

        // Suppress git credential prompts unconditionally. cube runs headless
        // and has no terminal to type into; a credential prompt would hang the
        // subprocess until run_with_timeout kills it. Setting this to "0"
        // makes git fail immediately with an auth error instead of hanging.
        cmd.env("GIT_TERMINAL_PROMPT", "0");
    }
}

impl CommandRunner for RealCommandRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError> {
        let mut cmd = Command::new(&invocation.program);
        cmd.args(&invocation.args).current_dir(&invocation.cwd);
        Self::configure_env(&mut cmd);
        for (k, v) in &invocation.env {
            cmd.env(k, v);
        }

        let output = cmd.output().map_err(CubeError::Io)?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(CubeError::CommandFailed {
                program: invocation.program.clone(),
                args: invocation.args.clone(),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }

    fn run_cleanup(&self, invocation: &CommandInvocation, timeout: Duration) -> Result<String, CubeError> {
        let output = self
            .run_with_timeout_capturing_output_and_heartbeat(
                invocation,
                &[],
                timeout,
                HEARTBEAT_INTERVAL,
                true,
                |elapsed, _| eprintln!("cube: cleanup still running ({}s elapsed)", elapsed.as_secs()),
            )?
            .ok_or_else(|| CubeError::CommandTimedOut {
                program: invocation.program.clone(),
                args: invocation.args.clone(),
                timeout_secs: timeout.as_secs(),
            })?;
        let combined = [
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
        if output.status.success() {
            Ok(combined)
        } else {
            Err(CubeError::CommandFailed {
                program: invocation.program.clone(),
                args: invocation.args.clone(),
                status: output.status.code(),
                stderr: combined,
            })
        }
    }

    fn run_with_timeout(&self, invocation: &CommandInvocation, timeout: Duration) -> Result<String, CubeError> {
        let program = invocation.program.clone();
        let args = invocation.args.join(" ");
        self.run_with_timeout_and_heartbeat(invocation, timeout, HEARTBEAT_INTERVAL, |elapsed, timeout| {
            eprintln!(
                "cube: still waiting on `{program} {args}` ({}s elapsed, {}s deadline)...",
                elapsed.as_secs(),
                timeout.as_secs(),
            );
        })
    }
}

impl RealCommandRunner {
    /// Run a command with a wall-clock timeout, returning the raw
    /// [`std::process::Output`] regardless of exit status (`Ok(Some(_))`),
    /// or `Ok(None)` if the timeout elapsed (the child is killed and reaped
    /// first). `Err` reports an I/O failure while spawning, waiting, or
    /// reading the command's output.
    ///
    /// Unlike [`CommandRunner::run_with_timeout`], failure is not inferred
    /// from a non-zero exit code and stdout is never discarded, so callers
    /// that need the raw output on a failing exit (e.g. the checkleft push
    /// gate, which echoes checkleft's own findings) use this instead of the
    /// `Result<String, CubeError>`-returning trait method.
    ///
    /// `env_remove` lists variable names to explicitly clear from the
    /// child's environment -- e.g. to override an ambient value with an
    /// *absence* rather than a fresh value, which [`CommandInvocation::env`]
    /// cannot express since it can only add.
    pub fn run_with_timeout_capturing_output(
        &self,
        invocation: &CommandInvocation,
        env_remove: &[&str],
        timeout: Duration,
    ) -> Result<Option<std::process::Output>, CubeError> {
        let program = invocation.program.clone();
        let args = invocation.args.join(" ");
        self.run_with_timeout_capturing_output_and_heartbeat(
            invocation,
            env_remove,
            timeout,
            HEARTBEAT_INTERVAL,
            false,
            move |elapsed, timeout| {
                eprintln!(
                    "cube: still waiting on `{program} {args}` ({}s elapsed, {}s deadline)...",
                    elapsed.as_secs(),
                    timeout.as_secs(),
                );
            },
        )
    }

    /// Core of [`RealCommandRunner::run_with_timeout_capturing_output`],
    /// with the heartbeat cadence and callback injectable so tests can
    /// verify heartbeats fire without waiting out a real 15s interval or
    /// scraping process stderr.
    fn run_with_timeout_capturing_output_and_heartbeat(
        &self,
        invocation: &CommandInvocation,
        env_remove: &[&str],
        timeout: Duration,
        heartbeat_interval: Duration,
        own_process_group: bool,
        mut on_heartbeat: impl FnMut(Duration, Duration),
    ) -> Result<Option<std::process::Output>, CubeError> {
        let mut cmd = Command::new(&invocation.program);
        cmd.args(&invocation.args)
            .current_dir(&invocation.cwd)
            .stdin(Stdio::null());
        Self::configure_env(&mut cmd);
        for (k, v) in &invocation.env {
            cmd.env(k, v);
        }
        for k in env_remove {
            cmd.env_remove(k);
        }

        let mut last_heartbeat = Duration::ZERO;
        match process_runner::output_bounded(
            &mut cmd,
            timeout,
            own_process_group,
            |_| {},
            |elapsed| {
                if elapsed.saturating_sub(last_heartbeat) >= heartbeat_interval {
                    on_heartbeat(elapsed, timeout);
                    last_heartbeat = elapsed;
                }
            },
        ) {
            Ok(output) => Ok(Some(output)),
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => Ok(None),
            Err(error) => Err(CubeError::Io(error)),
        }
    }

    /// Core of [`CommandRunner::run_with_timeout`], with the heartbeat
    /// cadence and callback injectable so tests can verify heartbeats fire
    /// without waiting out a real 15s interval or scraping process stderr.
    fn run_with_timeout_and_heartbeat(
        &self,
        invocation: &CommandInvocation,
        timeout: Duration,
        heartbeat_interval: Duration,
        on_heartbeat: impl FnMut(Duration, Duration),
    ) -> Result<String, CubeError> {
        match self.run_with_timeout_capturing_output_and_heartbeat(
            invocation,
            &[],
            timeout,
            heartbeat_interval,
            false,
            on_heartbeat,
        )? {
            None => Err(CubeError::CommandTimedOut {
                program: invocation.program.clone(),
                args: invocation.args.clone(),
                timeout_secs: timeout.as_secs(),
            }),
            Some(output) => {
                if output.status.success() {
                    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
                } else {
                    Err(CubeError::CommandFailed {
                        program: invocation.program.clone(),
                        args: invocation.args.clone(),
                        status: output.status.code(),
                        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_with_timeout_returns_output_for_fast_command() {
        let runner = RealCommandRunner;
        let dir = std::env::temp_dir();
        let out = runner
            .run_with_timeout(
                &RealCommandRunner::invocation(&dir, "echo", &["hello"]),
                Duration::from_secs(10),
            )
            .expect("fast command should succeed");
        assert_eq!(out, "hello");
    }

    #[test]
    fn run_with_timeout_kills_descendants_of_the_command() {
        let runner = RealCommandRunner;
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("survived");
        // The shell backgrounds a grandchild that would write the marker after
        // the deadline; killing only the direct child would let it.
        let script = format!("(sleep 1; touch '{}') & wait", marker.display());
        let err = runner
            .run_cleanup(
                &RealCommandRunner::invocation(dir.path(), "sh", &["-c", &script]),
                Duration::from_millis(200),
            )
            .expect_err("should time out");
        assert!(matches!(err, CubeError::CommandTimedOut { .. }), "got {err:?}");
        thread::sleep(Duration::from_millis(1800));
        assert!(!marker.exists(), "a descendant outlived the timeout");
    }

    #[test]
    fn cleanup_drain_is_bounded_when_a_descendant_holds_the_pipe() {
        let started = Instant::now();
        let err = RealCommandRunner
            .run_cleanup(
                &RealCommandRunner::invocation(&std::env::temp_dir(), "sh", &["-c", "sleep 60 & exit 0"]),
                Duration::from_millis(200),
            )
            .unwrap_err();
        assert!(matches!(err, CubeError::CommandTimedOut { .. }));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn only_cleanup_commands_lead_their_own_process_group() {
        // Use the shell builtin: process enumeration is denied by the test sandbox.
        let invocation = RealCommandRunner::invocation(
            &std::env::temp_dir(),
            "sh",
            &[
                "-c",
                "if kill -0 -$$ 2>/dev/null; then echo isolated; else echo inherited; fi",
            ],
        );
        assert_eq!(
            RealCommandRunner
                .run_with_timeout(&invocation, Duration::from_secs(5))
                .unwrap(),
            "inherited"
        );
        assert_eq!(
            RealCommandRunner
                .run_cleanup(&invocation, Duration::from_secs(5))
                .unwrap(),
            "isolated"
        );
    }

    #[test]
    fn run_with_timeout_kills_and_errors_on_slow_command() {
        let runner = RealCommandRunner;
        let dir = std::env::temp_dir();
        let started = Instant::now();
        let err = runner
            .run_with_timeout(
                &RealCommandRunner::invocation(&dir, "sleep", &["30"]),
                Duration::from_millis(200),
            )
            .expect_err("slow command should time out");
        let elapsed = started.elapsed();
        assert!(
            matches!(err, CubeError::CommandTimedOut { .. }),
            "expected CommandTimedOut, got {err:?}"
        );
        // Must return shortly after the deadline, not after the full sleep.
        assert!(
            elapsed < Duration::from_secs(5),
            "run_with_timeout returned in {elapsed:?}, expected to bail out near the 200ms deadline"
        );
    }

    #[test]
    fn run_with_timeout_emits_heartbeats_while_child_is_still_running() {
        let runner = RealCommandRunner;
        let dir = std::env::temp_dir();
        let heartbeats = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let heartbeats_cb = heartbeats.clone();
        let out = runner
            .run_with_timeout_and_heartbeat(
                &RealCommandRunner::invocation(&dir, "sleep", &["1"]),
                Duration::from_secs(10),
                Duration::from_millis(100),
                move |elapsed, timeout| {
                    heartbeats_cb.lock().unwrap().push((elapsed, timeout));
                },
            )
            .expect("command should complete within the timeout");
        assert_eq!(out, "");
        let recorded = heartbeats.lock().unwrap();
        assert!(
            !recorded.is_empty(),
            "expected at least one heartbeat while `sleep 1` was still running"
        );
        assert!(recorded.iter().all(|(_, timeout)| *timeout == Duration::from_secs(10)));
    }

    #[test]
    fn run_with_timeout_propagates_nonzero_exit() {
        let runner = RealCommandRunner;
        let dir = std::env::temp_dir();
        let err = runner
            .run_with_timeout(
                &RealCommandRunner::invocation(&dir, "false", &[]),
                Duration::from_secs(10),
            )
            .expect_err("`false` exits non-zero");
        assert!(
            matches!(err, CubeError::CommandFailed { .. }),
            "expected CommandFailed, got {err:?}"
        );
    }
}
