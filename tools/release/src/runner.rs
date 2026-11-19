use std::process::Stdio;
use std::time::{Duration, SystemTime};

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
fn run_with_timeout(
    mut process: std::process::Command,
    program: &str,
    timeout: Duration,
) -> std::result::Result<CommandOutput, RunnerError> {
    process.stdin(Stdio::null());
    match process_runner::output_blocking_timeout(&mut process, timeout) {
        Ok(output) => Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => Err(RunnerError::new(format!(
            "{program} did not finish within {} seconds and was stopped",
            timeout.as_secs()
        ))),
        Err(error) => Err(RunnerError::new(format!("failed to run {program}: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

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
