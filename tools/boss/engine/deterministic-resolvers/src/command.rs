//! Process-spawning seam. Resolvers depend on the [`CommandRunner`] trait
//! rather than calling `tokio::process::Command` directly, so their
//! decline/resolve logic can be unit-tested without spawning a real
//! `cargo`/`bazel` process.

use std::path::Path;

use async_trait::async_trait;

/// Minimal captured shape of a finished child process — just enough for a
/// resolver to decide success/failure and report a reason.
#[derive(Debug, Clone)]
pub(crate) struct CommandOutput {
    pub(crate) success: bool,
    pub(crate) code: Option<i32>,
    pub(crate) stderr: String,
}

#[async_trait]
pub(crate) trait CommandRunner: Send + Sync {
    async fn run(&self, program: &str, args: &[&str], cwd: &Path) -> std::io::Result<CommandOutput>;
}

pub(crate) struct RealCommandRunner;

#[async_trait]
impl CommandRunner for RealCommandRunner {
    async fn run(&self, program: &str, args: &[&str], cwd: &Path) -> std::io::Result<CommandOutput> {
        let output = tokio::process::Command::new(program)
            .args(args)
            .current_dir(cwd)
            .output()
            .await?;
        Ok(CommandOutput {
            success: output.status.success(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

#[cfg(test)]
pub(crate) struct FakeCommandRunner {
    outcome: std::sync::Mutex<Option<std::io::Result<CommandOutput>>>,
    pub(crate) calls: std::sync::Mutex<Vec<(String, Vec<String>, std::path::PathBuf)>>,
}

#[cfg(test)]
impl FakeCommandRunner {
    pub(crate) fn success() -> Self {
        Self::with_outcome(Ok(CommandOutput {
            success: true,
            code: Some(0),
            stderr: String::new(),
        }))
    }

    pub(crate) fn failure(stderr: &str) -> Self {
        Self::with_outcome(Ok(CommandOutput {
            success: false,
            code: Some(1),
            stderr: stderr.to_owned(),
        }))
    }

    pub(crate) fn spawn_error() -> Self {
        Self::with_outcome(Err(std::io::Error::other("program not found")))
    }

    fn with_outcome(outcome: std::io::Result<CommandOutput>) -> Self {
        Self {
            outcome: std::sync::Mutex::new(Some(outcome)),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl CommandRunner for FakeCommandRunner {
    async fn run(&self, program: &str, args: &[&str], cwd: &Path) -> std::io::Result<CommandOutput> {
        self.calls.lock().unwrap().push((
            program.to_owned(),
            args.iter().map(|s| s.to_string()).collect(),
            cwd.to_path_buf(),
        ));
        self.outcome
            .lock()
            .unwrap()
            .take()
            .expect("FakeCommandRunner outcome consumed more than once")
    }
}
