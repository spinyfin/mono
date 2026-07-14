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

    /// Unwraps the `Declined` reason string, panicking on `Ok`. Every
    /// failure branch of `run_or_decline` returns `Err(Declined { .. })`;
    /// this keeps each assertion focused on the observable reason text.
    fn declined_reason(result: Result<(), ResolveOutcome>) -> String {
        match result {
            Err(ResolveOutcome::Declined { reason }) => reason,
            other => panic!("expected Err(Declined), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn success_returns_ok() {
        let runner = FakeCommandRunner::success();
        let result = run_or_decline(&runner, Path::new("/w"), "cargo", &["generate-lockfile"], "").await;
        assert!(result.is_ok(), "successful command should return Ok, got {result:?}");
    }

    #[tokio::test]
    async fn spawn_error_declines_naming_the_program() {
        let runner = FakeCommandRunner::spawn_error();
        let reason = declined_reason(run_or_decline(&runner, Path::new("/w"), "cargo", &["build"], "").await);
        assert!(reason.contains("failed to spawn"), "reason was: {reason}");
        assert!(
            reason.contains("cargo"),
            "reason should name the program, was: {reason}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_with_stderr_reports_command_code_and_stderr() {
        let runner = FakeCommandRunner::failure("manifest is invalid");
        let reason = declined_reason(
            run_or_decline(
                &runner,
                Path::new("/w"),
                "cargo",
                &["generate-lockfile", "--offline"],
                "",
            )
            .await,
        );
        assert!(
            reason.contains("cargo"),
            "reason should name the program, was: {reason}"
        );
        assert!(
            reason.contains("generate-lockfile --offline"),
            "reason should include the joined args, was: {reason}"
        );
        // `code` is `Some(1)` in `FakeCommandRunner::failure`, formatted via `{:?}`.
        assert!(
            reason.contains("Some(1)"),
            "reason should include the exit code, was: {reason}"
        );
        assert!(
            reason.contains("manifest is invalid"),
            "reason should include stderr, was: {reason}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_with_empty_stderr_uses_no_stderr_placeholder() {
        // The one branch no lockfile/recipe resolver test exercises: a
        // command that fails but writes nothing to stderr falls back to the
        // literal "(no stderr)" placeholder.
        let runner = FakeCommandRunner::failure("");
        let reason = declined_reason(run_or_decline(&runner, Path::new("/w"), "cargo", &["build"], "").await);
        assert!(
            reason.contains("(no stderr)"),
            "reason should use the placeholder, was: {reason}"
        );
        assert!(
            !reason.contains("manifest"),
            "sanity: empty stderr should not leak unrelated text, was: {reason}"
        );
    }

    #[tokio::test]
    async fn reason_prefix_is_prepended_verbatim() {
        let prefix = "recipe \"schema\" (verify_command): ";

        let runner = FakeCommandRunner::failure("boom");
        let with_prefix = declined_reason(run_or_decline(&runner, Path::new("/w"), "cargo", &["build"], prefix).await);
        assert!(
            with_prefix.starts_with(prefix),
            "reason should start with the verbatim prefix, was: {with_prefix}"
        );

        // Empty prefix adds nothing: the reason starts with the command
        // backtick rather than any prefix text.
        let runner = FakeCommandRunner::failure("boom");
        let without_prefix = declined_reason(run_or_decline(&runner, Path::new("/w"), "cargo", &["build"], "").await);
        assert!(
            without_prefix.starts_with('`'),
            "empty prefix should add no leading text, was: {without_prefix}"
        );
    }
}
