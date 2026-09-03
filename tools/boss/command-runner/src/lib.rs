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

    /// True only for [`RealCommandRunner`] — the runner that actually execs a
    /// subprocess. `false` by default, so every fake/stub/scripted runner
    /// used in tests (there is no other kind in this codebase) reports itself
    /// as harmless without needing to implement this.
    ///
    /// Safety guards that must refuse a real subprocess exec from a test
    /// process (e.g. `boss-tmux`'s legacy-label-server constructor) key off
    /// this instead of `boss_log_files::is_test_process()` alone: a fake
    /// runner can never reach a live server no matter what path or label it
    /// is pointed at, so gating on the runner's own realness — rather than
    /// refusing unconditionally — closes the actual hazard without breaking
    /// the many existing tests that exercise real server-selection logic
    /// through an injected fake.
    fn is_real(&self) -> bool {
        false
    }
}

/// Locale environment variables, in the precedence order POSIX gives them.
const LOCALE_VARS: [&str; 3] = ["LC_ALL", "LC_CTYPE", "LANG"];

/// Charset-only locale forced onto children when this process has none.
/// `LC_CTYPE` rather than `LANG`/`LC_ALL` so we pin the character encoding
/// without imposing a language or region on the child.
const FALLBACK_LC_CTYPE: (&str, &str) = ("LC_CTYPE", "UTF-8");

/// True when `value` names a UTF-8 charset — either a bare `UTF-8` or the
/// `<locale>.UTF-8` form. Case- and separator-insensitive, since `en_US.utf8`
/// and `en_US.UTF-8` are both in circulation.
fn is_utf8_locale(value: &str) -> bool {
    let charset = value.rsplit('.').next().unwrap_or(value);
    let normalized: String = charset
        .chars()
        .filter(|c| *c != '-' && *c != '_')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    normalized == "utf8"
}

/// The locale to force onto a child, or `None` when this process already has
/// a UTF-8 one to pass down.
///
/// Boss is normally launched by LaunchServices (Dock, Finder, `open`), which
/// supplies no `LANG`/`LC_*` at all — a terminal launch is the exception, not
/// the rule. A child that inherits no locale falls back to the C locale, and
/// tmux in particular then treats its client as non-UTF-8 and runs every line
/// it prints through `utf8_sanitize()`, which rewrites each non-printable byte
/// to `_`. That silently corrupts the TAB delimiter in `list-sessions -F`
/// output and mangles any pane capture containing control characters. Forcing
/// a UTF-8 `LC_CTYPE` keeps tmux's output byte-exact however Boss was started.
fn forced_locale() -> Option<(&'static str, &'static str)> {
    let already_utf8 = LOCALE_VARS
        .iter()
        .any(|name| std::env::var(name).ok().is_some_and(|value| is_utf8_locale(&value)));
    (!already_utf8).then_some(FALLBACK_LC_CTYPE)
}

/// What this process inherited for the locale, and what children will get.
///
/// Exposed so a host can log its own environment at startup rather than
/// leaving it to be reconstructed later. Incident 006 was diagnosed by
/// inferring the engine's locale from a *statistical* argument about how
/// often an unrelated parse failed, because nothing recorded the value
/// itself; see `tools/boss/docs/postmortems/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleDiagnostics {
    /// Inherited `LC_ALL`, `LC_CTYPE`, `LANG`, in that order. `None` means
    /// the variable is unset — distinct from `Some("")`, which some
    /// launchers do set and which POSIX treats as unset.
    pub inherited: [(&'static str, Option<String>); 3],
    /// True when at least one inherited variable names a UTF-8 charset.
    pub has_utf8_locale: bool,
    /// The variable and value forced onto children, if any.
    pub forced: Option<(&'static str, &'static str)>,
}

impl LocaleDiagnostics {
    pub fn probe() -> Self {
        let inherited = LOCALE_VARS.map(|name| (name, std::env::var(name).ok()));
        Self {
            inherited,
            has_utf8_locale: forced_locale().is_none(),
            forced: forced_locale(),
        }
    }

    /// Compact `LC_ALL=…,LC_CTYPE=<unset>,LANG=…` rendering for one log field.
    pub fn inherited_summary(&self) -> String {
        self.inherited
            .iter()
            .map(|(name, value)| match value {
                Some(value) => format!("{name}={value}"),
                None => format!("{name}=<unset>"),
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Runs commands through Tokio's process API.
#[derive(Debug, Default)]
pub struct RealCommandRunner;

#[async_trait]
impl CommandRunner for RealCommandRunner {
    fn is_real(&self) -> bool {
        true
    }

    async fn run(&self, program: &Path, args: &[OsString], cwd: Option<&Path>) -> std::io::Result<CommandOutput> {
        let mut command = tokio::process::Command::new(program);
        command.args(args);
        if let Some((name, value)) = forced_locale() {
            command.env(name, value);
        }
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
        if let Some((name, value)) = forced_locale() {
            command.env(name, value);
        }
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

    #[test]
    fn utf8_charsets_are_recognized_in_every_spelling_in_circulation() {
        for value in ["UTF-8", "utf8", "en_US.UTF-8", "en_GB.utf8", "C.UTF-8"] {
            assert!(is_utf8_locale(value), "{value} should read as UTF-8");
        }
    }

    #[test]
    fn non_utf8_charsets_are_not_mistaken_for_utf8() {
        for value in ["C", "POSIX", "en_US.ISO8859-1", "", "utf"] {
            assert!(!is_utf8_locale(value), "{value} should not read as UTF-8");
        }
    }

    /// The child must actually receive a UTF-8 `LC_CTYPE` when this process
    /// has no locale of its own — the LaunchServices case. Asserted through a
    /// real spawn rather than on `forced_locale()` alone, so the wiring into
    /// `Command::env` is covered too.
    #[tokio::test]
    async fn a_child_is_given_a_utf8_ctype_when_this_process_has_no_locale() {
        if forced_locale().is_none() {
            // This test process inherited a UTF-8 locale (the usual case when
            // run from a terminal); there is nothing to force.
            return;
        }
        let runner = RealCommandRunner;
        let output = runner
            .run(
                Path::new("/bin/sh"),
                &[OsString::from("-c"), OsString::from("printf %s \"$LC_CTYPE\"")],
                None,
            )
            .await
            .expect("spawn shell");
        assert!(is_utf8_locale(&output.stdout), "child LC_CTYPE was {:?}", output.stdout);
    }
}
