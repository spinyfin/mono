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

    /// Runs a command while supplying its standard input. Runners which do
    /// not model stdin may leave this unsupported; callers that require it
    /// must use a runner which implements this method.
    async fn run_with_stdin(
        &self,
        _program: &Path,
        _args: &[OsString],
        _cwd: Option<&Path>,
        _stdin: &[u8],
    ) -> std::io::Result<CommandOutput> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "command runner does not support stdin",
        ))
    }
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

    async fn run_with_stdin(
        &self,
        program: &Path,
        args: &[OsString],
        cwd: Option<&Path>,
        stdin: &[u8],
    ) -> std::io::Result<CommandOutput> {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        let mut command = tokio::process::Command::new(program);
        // Match `run`: capture stdout/stderr so callers get diagnostics in
        // CommandOutput and the child does not inherit (and pollute) the
        // engine process's descriptors.
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn()?;
        if let Some(mut child_stdin) = child.stdin.take() {
            child_stdin.write_all(stdin).await?;
        }
        let output = child.wait_with_output().await?;
        Ok(CommandOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;

    #[tokio::test]
    async fn run_with_stdin_captures_stdout_and_stderr() {
        let runner = RealCommandRunner;
        let output = runner
            .run_with_stdin(
                Path::new("/bin/sh"),
                &[OsString::from("-c"), OsString::from("cat; echo out; echo err >&2")],
                None,
                b"from-stdin",
            )
            .await
            .expect("spawn shell");
        assert!(output.success, "stderr={}", output.stderr);
        assert_eq!(output.stdout, "from-stdinout\n");
        assert_eq!(output.stderr, "err\n");
    }
}
