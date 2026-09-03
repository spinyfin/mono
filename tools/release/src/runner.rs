use std::time::Duration;

/// One subprocess invocation, represented as data so tests can inject a
/// captured runner instead of executing programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
}

impl Command {
    #[must_use]
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
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
}

/// Production command runner for the release CLI.
#[derive(Debug, Default)]
pub struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, command: &Command) -> std::result::Result<CommandOutput, RunnerError> {
        let output = std::process::Command::new(&command.program)
            .args(&command.args)
            .output()
            .map_err(|error| RunnerError::new(format!("failed to run {}: {error}", command.program)))?;

        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
