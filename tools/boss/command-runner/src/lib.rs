//! Shared asynchronous process runner for Boss components.

use std::ffi::OsString;
use std::path::Path;

use async_trait::async_trait;

/// Captured result of one command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Process-spawning seam for components that construct commands.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, program: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput>;
}

/// Runs commands through Tokio's process API.
#[derive(Debug, Default)]
pub struct RealCommandRunner;

#[async_trait]
impl CommandRunner for RealCommandRunner {
    async fn run(&self, program: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        let mut command = tokio::process::Command::new(program);
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command.output().await?;
        Ok(CommandOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
