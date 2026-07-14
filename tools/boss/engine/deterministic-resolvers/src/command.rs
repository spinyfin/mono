//! Process-spawning seam. Resolvers depend on the [`CommandRunner`] trait
//! rather than calling `tokio::process::Command` directly, so their
//! decline/resolve logic can be unit-tested without spawning a real
//! `cargo`/`bazel` process.

use std::path::Path;

use async_trait::async_trait;

use crate::ResolveOutcome;

/// Minimal captured shape of a finished child process — just enough for a
/// resolver to decide success/failure and report a reason.
#[derive(Debug, Clone)]
pub(crate) struct CommandOutput {
    pub(crate) success: bool,
    pub(crate) code: Option<i32>,
    pub(crate) stderr: String,
}

/// Runs one command, mapping a spawn failure or non-zero exit to a
/// `Declined` outcome — the shared "run a command, decide success/failure"
/// core used by every discard→run→verify strategy (the built-in lockfile
/// resolvers in `lockfile.rs` and the generic [`crate::RecipeResolver`] in
/// `resolvers/recipe.rs`). `reason_prefix` is prepended verbatim to the
/// decline reason so each caller can identify which command/strategy
/// failed (e.g. `"recipe \"schema\" (verify_command): "`); pass `""` for
/// no prefix.
pub(crate) async fn run_or_decline(
    runner: &dyn CommandRunner,
    dir: &Path,
    program: &str,
    args: &[&str],
    reason_prefix: &str,
) -> Result<(), ResolveOutcome> {
    let output = match runner.run(program, args, dir).await {
        Ok(output) => output,
        Err(e) => {
            return Err(ResolveOutcome::Declined {
                reason: format!("{reason_prefix}failed to spawn `{program}`: {e}"),
            });
        }
    };

    if !output.success {
        let stderr = if output.stderr.is_empty() {
            "(no stderr)"
        } else {
            &output.stderr
        };
        return Err(ResolveOutcome::Declined {
            reason: format!(
                "{reason_prefix}`{program} {}` exited {:?}: {stderr}",
                args.join(" "),
                output.code
            ),
        });
    }

    Ok(())
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
    /// Outcomes returned in order, one per call. A resolver that issues
    /// more calls than outcomes were queued panics (test bug, not a
    /// production path).
    outcomes: std::sync::Mutex<std::collections::VecDeque<std::io::Result<CommandOutput>>>,
    /// File to (re)write in `cwd` when a call succeeds, simulating a real
    /// `cargo`/`bazel` invocation actually regenerating the lockfile.
    writes_file: Option<(String, String)>,
    pub(crate) calls: std::sync::Mutex<Vec<(String, Vec<String>, std::path::PathBuf)>>,
}

#[cfg(test)]
impl FakeCommandRunner {
    /// Succeeds without touching the filesystem — useful for exercising the
    /// "command exits 0 but never wrote the lockfile" case.
    pub(crate) fn success() -> Self {
        Self::with_outcome(Ok(CommandOutput {
            success: true,
            code: Some(0),
            stderr: String::new(),
        }))
    }

    /// Succeeds and writes `filename` (with `contents`) into `cwd`, the way
    /// `cargo generate-lockfile`/`bazel mod deps` would in reality.
    pub(crate) fn success_writing_file(filename: &str, contents: &str) -> Self {
        let mut runner = Self::success();
        runner.writes_file = Some((filename.to_owned(), contents.to_owned()));
        runner
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
        Self::sequence(vec![outcome])
    }

    /// Returns `outcomes[0]` on the first call, `outcomes[1]` on the
    /// second, and so on — for resolvers (like the recipe resolver)
    /// that issue more than one command per `resolve` call.
    pub(crate) fn sequence(outcomes: Vec<std::io::Result<CommandOutput>>) -> Self {
        Self {
            outcomes: std::sync::Mutex::new(outcomes.into_iter().collect()),
            writes_file: None,
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
        let outcome = self
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("FakeCommandRunner ran out of queued outcomes");
        if let (Ok(output), Some((filename, contents))) = (&outcome, &self.writes_file)
            && output.success
        {
            std::fs::write(cwd.join(filename), contents)?;
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResolveOutcome;

    /// Non-zero exit with EMPTY stderr must fall back to the `(no stderr)`
    /// placeholder in the decline reason. This branch (command.rs:46-50) is
    /// otherwise never exercised — every other failing call passes real
    /// stderr text.
    #[tokio::test]
    async fn declines_with_no_stderr_placeholder_when_stderr_empty() {
        let runner = FakeCommandRunner::failure("");
        let result = run_or_decline(&runner, Path::new("/tmp/ws"), "cargo", &["generate-lockfile"], "").await;

        let ResolveOutcome::Declined { reason } = result.expect_err("expected a decline") else {
            panic!("expected Declined");
        };
        assert!(
            reason.contains("(no stderr)"),
            "reason should use the placeholder, got: {reason}"
        );
    }

    /// A non-zero exit with real stderr surfaces the stderr text, the exit
    /// code, and the joined args string in the decline reason.
    #[tokio::test]
    async fn declines_with_stderr_code_and_args_on_failure() {
        let runner = FakeCommandRunner::failure("boom: dependency conflict");
        let result = run_or_decline(
            &runner,
            Path::new("/tmp/ws"),
            "cargo",
            &["generate-lockfile", "--offline"],
            "",
        )
        .await;

        let ResolveOutcome::Declined { reason } = result.expect_err("expected a decline") else {
            panic!("expected Declined");
        };
        assert!(
            reason.contains("boom: dependency conflict"),
            "reason should contain stderr, got: {reason}"
        );
        assert!(
            reason.contains("Some(1)"),
            "reason should contain the exit code, got: {reason}"
        );
        assert!(
            reason.contains("generate-lockfile --offline"),
            "reason should contain the joined args, got: {reason}"
        );
    }

    /// A spawn failure (the runner itself errors) reports `failed to spawn`
    /// with the program name and the underlying error text.
    #[tokio::test]
    async fn declines_with_spawn_error_details() {
        let runner = FakeCommandRunner::spawn_error();
        let result = run_or_decline(&runner, Path::new("/tmp/ws"), "bazel", &["mod", "deps"], "").await;

        let ResolveOutcome::Declined { reason } = result.expect_err("expected a decline") else {
            panic!("expected Declined");
        };
        assert!(
            reason.contains("failed to spawn `bazel`"),
            "reason should name the program, got: {reason}"
        );
        assert!(
            reason.contains("program not found"),
            "reason should contain the underlying error, got: {reason}"
        );
    }

    /// A successful (exit 0) command yields `Ok(())`.
    #[tokio::test]
    async fn returns_ok_on_success() {
        let runner = FakeCommandRunner::success();
        let result = run_or_decline(&runner, Path::new("/tmp/ws"), "cargo", &["generate-lockfile"], "").await;

        assert_eq!(result, Ok(()));
    }

    /// A non-empty `reason_prefix` is prepended verbatim to the decline
    /// reason, letting each caller identify which command/strategy failed.
    #[tokio::test]
    async fn prepends_reason_prefix_verbatim() {
        let runner = FakeCommandRunner::failure("nope");
        let result = run_or_decline(
            &runner,
            Path::new("/tmp/ws"),
            "cargo",
            &["check"],
            "recipe \"schema\" (verify_command): ",
        )
        .await;

        let ResolveOutcome::Declined { reason } = result.expect_err("expected a decline") else {
            panic!("expected Declined");
        };
        assert!(
            reason.starts_with("recipe \"schema\" (verify_command): "),
            "reason should start with the prefix, got: {reason}"
        );
    }
}
