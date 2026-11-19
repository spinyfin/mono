use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};

/// How often a time-bounded command is checked for completion.
const TIMEOUT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One subprocess invocation, represented as data so tests can inject a
/// captured runner instead of executing programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    /// Longest the command may run before the runner kills it and reports a
    /// failure. `None` waits for as long as the command takes.
    pub timeout: Option<Duration>,
}

impl Command {
    #[must_use]
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            timeout: None,
        }
    }

    /// Bounds how long the command may run. Meant for network transfers, where
    /// a stalled connection would otherwise hold the release open indefinitely.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// Output captured from a subprocess invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Failure to start or communicate with a command runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerError {
    message: String,
}

impl RunnerError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RunnerError {}

/// Synchronous subprocess seam for release-state queries.
///
/// This deliberately stays local to the release crate: Boss's similarly
/// named runner is asynchronous and Tokio-backed, while Cube's is coupled to
/// its command invocation and error types. Neither is a suitable dependency
/// for this deterministic, synchronous library boundary.
pub trait CommandRunner {
    fn run(&self, command: &Command) -> std::result::Result<CommandOutput, RunnerError>;

    /// Waits between retry attempts. Production runners use the default;
    /// fixtures override it to keep tests deterministic and fast.
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    /// The current wall-clock time, used to date the claim a run records on a
    /// draft release. Fixtures override it to keep those dates deterministic.
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Production command runner for the release CLI.
#[derive(Debug, Default)]
pub struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, command: &Command) -> std::result::Result<CommandOutput, RunnerError> {
        let mut process = std::process::Command::new(&command.program);
        process.args(&command.args);
        let Some(timeout) = command.timeout else {
            let output = process
                .output()
                .map_err(|error| RunnerError::new(format!("failed to run {}: {error}", command.program)))?;
            return Ok(CommandOutput {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        };
        run_with_timeout(process, &command.program, timeout)
    }
}

/// Runs `process` to completion, or kills it once `timeout` has passed.
///
/// Both pipes are drained on their own threads so a command that fills one
/// cannot block before it exits. After a kill the readers are left to finish
/// on their own instead of being joined: a descendant that inherited a pipe
/// could otherwise keep this call waiting past the bound it exists to enforce.
fn run_with_timeout(
    mut process: std::process::Command,
    program: &str,
    timeout: Duration,
) -> std::result::Result<CommandOutput, RunnerError> {
    let mut child = process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| RunnerError::new(format!("failed to run {program}: {error}")))?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunnerError::new(format!(
                    "{program} did not finish within {} seconds and was stopped",
                    timeout.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(TIMEOUT_POLL_INTERVAL),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunnerError::new(format!("failed to wait for {program}: {error}")));
            }
        }
    };

    Ok(CommandOutput {
        success: status.success(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bounded_command_that_finishes_in_time_returns_its_output() {
        let command =
            Command::new("sh", ["-c", "printf out; printf err >&2; exit 3"]).with_timeout(Duration::from_secs(30));

        let output = ProcessCommandRunner.run(&command).expect("command runs");

        assert!(!output.success);
        assert_eq!(output.stdout, "out");
        assert_eq!(output.stderr, "err");
    }

    #[test]
    fn a_bounded_command_that_overruns_is_stopped_and_reported() {
        let command = Command::new("sleep", ["30"]).with_timeout(Duration::from_millis(200));
        let started = Instant::now();

        let error = ProcessCommandRunner.run(&command).expect_err("overrun must fail");

        assert!(error.to_string().contains("did not finish within"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the command was not stopped at its bound: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn an_unbounded_command_still_captures_its_output() {
        let output = ProcessCommandRunner
            .run(&Command::new("sh", ["-c", "printf plain"]))
            .expect("command runs");

        assert!(output.success);
        assert_eq!(output.stdout, "plain");
    }
}
