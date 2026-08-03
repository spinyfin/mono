use std::ffi::OsString;
use std::path::Path;

use async_trait::async_trait;

/// Captured result of one tmux invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Process-spawning seam for [`crate::Tmux`].
///
/// Keeping this boundary small lets engine tests exercise every tmux command
/// shape without depending on a tmux server or a terminal.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, program: &Path, args: &[OsString]) -> std::io::Result<CommandOutput>;
}

/// Runs tmux through Tokio's process API.
#[derive(Debug, Default)]
pub struct RealCommandRunner;

#[async_trait]
impl CommandRunner for RealCommandRunner {
    async fn run(&self, program: &Path, args: &[OsString]) -> std::io::Result<CommandOutput> {
        let output = tokio::process::Command::new(program).args(args).output().await?;
        Ok(CommandOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
