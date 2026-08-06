//! Typed control surface for Boss's private tmux server.
//!
//! Every command is scoped to `tmux -L boss`, so Boss's sessions are isolated
//! from any tmux server running on the default socket. The process runner is injectable so callers
//! can test command construction without a live tmux daemon.

mod types;

pub use boss_command_runner::{CommandOutput, CommandRunner, RealCommandRunner};
pub use types::{
    DEFAULT_SEND_CHUNK_BYTES, DEFAULT_SEND_CHUNK_DELAY, DisplayField, MINIMUM_VERSION, NewSession, SERVER_LABEL,
    Session, TmuxVersion,
};

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::time::sleep;

use crate::types::validate_value;

/// Environment variable carrying the durable spawn token that identifies
/// which execution a session belongs to. The sole thing
/// [`Tmux::kill_session_verified`] trusts to decide whether a session is
/// the caller's to destroy.
const BOSS_SPAWN_TOKEN_ENV: &str = "BOSS_SPAWN_TOKEN";

/// Outcome of a successful [`Tmux::kill_session_verified`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillSessionOutcome {
    /// The session existed, its live token matched, and it has been
    /// destroyed.
    Killed,
    /// No session by that name exists on the private server (or it carries
    /// no `BOSS_SPAWN_TOKEN` at all) — treated as an already-completed
    /// teardown, not an error, so repeated calls stay idempotent.
    Absent,
}

/// Failure returned by [`Tmux::kill_session_verified`].
#[derive(Debug)]
pub enum KillSessionError {
    /// A session by that name exists, but its live token does not match
    /// `expected_token`. Refused: the session currently answering to that
    /// name is not the one the caller recorded, most likely because the
    /// original session was already destroyed and the name recycled onto a
    /// different execution. Nothing was signalled or killed.
    TokenMismatch {
        session: String,
        expected: String,
        actual: String,
    },
    /// The underlying `tmux` invocation failed.
    Tmux(anyhow::Error),
}

impl std::fmt::Display for KillSessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TokenMismatch {
                session,
                expected,
                actual,
            } => write!(
                formatter,
                "refusing to kill tmux session {session:?}: live token {actual:?} does not match the \
                 expected {expected:?}",
            ),
            Self::Tmux(err) => write!(formatter, "{err:#}"),
        }
    }
}

impl std::error::Error for KillSessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TokenMismatch { .. } => None,
            Self::Tmux(err) => err.source(),
        }
    }
}

impl From<anyhow::Error> for KillSessionError {
    fn from(err: anyhow::Error) -> Self {
        Self::Tmux(err)
    }
}

/// Handle for one resolved tmux executable and Boss's private server.
#[derive(Clone)]
pub struct Tmux {
    program: PathBuf,
    runner: Arc<dyn CommandRunner>,
}

impl std::fmt::Debug for Tmux {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Tmux")
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

impl Tmux {
    /// Resolves tmux once to a canonical absolute path for durable use.
    pub fn resolve() -> Result<Self> {
        let program = which::which("tmux").context("locating tmux on PATH")?;
        let program =
            std::fs::canonicalize(&program).with_context(|| format!("canonicalizing tmux path {program:?}"))?;
        Self::from_path(program)
    }

    /// Creates a controller for an already-resolved absolute executable path.
    pub fn from_path(program: impl Into<PathBuf>) -> Result<Self> {
        Self::with_runner(program, Arc::new(RealCommandRunner))
    }

    /// Creates a controller with a test or host-specific process runner.
    pub fn with_runner(program: impl Into<PathBuf>, runner: Arc<dyn CommandRunner>) -> Result<Self> {
        let program = program.into();
        if !program.is_absolute() {
            bail!("tmux executable path must be absolute: {program:?}");
        }
        Ok(Self { program, runner })
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Probes the resolved executable's version before the engine accepts work.
    pub async fn version(&self) -> Result<TmuxVersion> {
        let mut args = self.server_args();
        args.push("-V".into());
        let output = self.invoke(args).await?;
        TmuxVersion::parse(&output.stdout)
    }

    /// Creates a detached, single-command session with environment set atomically.
    pub async fn new_session(&self, session: &NewSession) -> Result<()> {
        session.validate()?;
        let mut args = self.server_args();
        args.extend([
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            session.name.clone().into(),
        ]);
        for (name, value) in &session.environment {
            args.extend(["-e".into(), format!("{name}={value}").into()]);
        }
        args.extend([
            "-c".into(),
            session.working_directory.clone().into_os_string(),
            session.command.clone().into(),
        ]);
        self.invoke(args).await.map(|_| ())
    }

    /// Lists sessions and their inexpensive token mirrors in one server call.
    pub async fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut args = self.server_args();
        args.extend([
            "list-sessions".into(),
            "-F".into(),
            "#{session_name}\t#{@boss_spawn_token}".into(),
        ]);
        let output = self.run(&args).await?;
        if !output.success {
            if is_absent_session_stderr(&output.stderr) {
                return Ok(Vec::new());
            }
            return command_failed(&args, &output);
        }
        output
            .stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(parse_session)
            .collect()
    }

    /// Reads a session environment variable. An unset variable is `None`.
    pub async fn show_environment(&self, session: &str, name: &str) -> Result<Option<String>> {
        validate_value("session name", session)?;
        validate_value("environment name", name)?;
        let mut args = self.server_args();
        args.extend(["show-environment".into(), "-t".into(), session.into(), name.into()]);
        let output = self.run(&args).await?;
        if output.success {
            let value = output.stdout.trim_end();
            let prefix = format!("{name}=");
            return value
                .strip_prefix(&prefix)
                .map(|value| Some(value.to_owned()))
                .ok_or_else(|| anyhow::anyhow!("unexpected tmux environment output for {name:?}: {value:?}"));
        }
        if output.stderr.contains("unknown variable")
            || output.stderr.contains("environment variable not found")
            || is_absent_session_stderr(&output.stderr)
        {
            return Ok(None);
        }
        command_failed(&args, &output)
    }

    /// Sets a per-session tmux option such as `@boss_spawn_token`.
    pub async fn set_option(&self, session: &str, option: &str, value: &str) -> Result<()> {
        validate_value("session name", session)?;
        validate_value("option name", option)?;
        validate_value("option value", value)?;
        let mut args = self.server_args();
        args.extend([
            "set-option".into(),
            "-t".into(),
            session.into(),
            option.into(),
            value.into(),
        ]);
        self.invoke(args).await.map(|_| ())
    }

    /// Reads one per-session tmux option, returning `None` when it is unset.
    pub async fn show_option(&self, session: &str, option: &str) -> Result<Option<String>> {
        validate_value("session name", session)?;
        validate_value("option name", option)?;
        let mut args = self.server_args();
        args.extend([
            "show-options".into(),
            "-v".into(),
            "-t".into(),
            session.into(),
            option.into(),
        ]);
        let output = self.run(&args).await?;
        if output.success {
            return Ok(Some(output.stdout.trim_end().to_owned()));
        }
        if output.stderr.contains("invalid option") || output.stderr.contains("unknown option") {
            return Ok(None);
        }
        command_failed(&args, &output)
    }

    /// Sends literal input in bounded chunks, then submits it in a separate call.
    pub async fn send_keys(&self, session: &str, text: &str) -> Result<()> {
        validate_value("session name", session)?;
        if text.contains('\0') {
            bail!("tmux key input cannot contain NUL");
        }
        for chunk in utf8_chunks(text, DEFAULT_SEND_CHUNK_BYTES) {
            let chunk = if chunk == ";" { "\\;" } else { chunk };
            let mut args = self.server_args();
            args.extend([
                "send-keys".into(),
                "-t".into(),
                session.into(),
                "-l".into(),
                "--".into(),
                chunk.into(),
            ]);
            self.invoke(args).await?;
            sleep(DEFAULT_SEND_CHUNK_DELAY).await;
        }
        let mut args = self.server_args();
        args.extend(["send-keys".into(), "-t".into(), session.into(), "C-m".into()]);
        self.invoke(args).await.map(|_| ())
    }

    /// Captures the visible text of a detached session's pane.
    pub async fn capture_pane(&self, session: &str) -> Result<String> {
        validate_value("session name", session)?;
        let mut args = self.server_args();
        args.extend(["capture-pane".into(), "-p".into(), "-t".into(), session.into()]);
        Ok(self.invoke(args).await?.stdout)
    }

    /// Destroys `session` only after confirming its live `BOSS_SPAWN_TOKEN`
    /// matches `expected_token` exactly — the sole sanctioned way to kill a
    /// Boss-owned tmux session. This crate deliberately exposes no "kill by
    /// name alone" entry point: a caller that has not durably recorded the
    /// token it expects cannot destroy anything through this API.
    ///
    /// `Ok(Absent)` means the session was already gone (or never carried a
    /// Boss token), which is treated as an idempotent success rather than
    /// an error so repeated teardown calls stay safe.
    /// `Err(KillSessionError::TokenMismatch)` means a session answers to
    /// `session`, but with a *different* token: refuses to touch it, since
    /// destroying it would tear down a worker this caller does not own —
    /// this is the guard against a session name recycled onto a different
    /// execution after the original session was destroyed.
    pub async fn kill_session_verified(
        &self,
        session: &str,
        expected_token: &str,
    ) -> std::result::Result<KillSessionOutcome, KillSessionError> {
        validate_value("session name", session)?;
        validate_value("expected token", expected_token)?;
        match self.show_environment(session, BOSS_SPAWN_TOKEN_ENV).await? {
            None => Ok(KillSessionOutcome::Absent),
            Some(actual) if actual == expected_token => {
                self.kill_session_unchecked(session).await?;
                Ok(KillSessionOutcome::Killed)
            }
            Some(actual) => Err(KillSessionError::TokenMismatch {
                session: session.to_owned(),
                expected: expected_token.to_owned(),
                actual,
            }),
        }
    }

    /// Low-level `kill-session -t <name>`, with no identity verification.
    /// Private: [`Self::kill_session_verified`] is this crate's only public
    /// teardown entry point, by design — see its doc comment.
    async fn kill_session_unchecked(&self, session: &str) -> Result<()> {
        validate_value("session name", session)?;
        let mut args = self.server_args();
        args.extend(["kill-session".into(), "-t".into(), session.into()]);
        self.invoke(args).await.map(|_| ())
    }

    /// Reads one known pane/window field without parsing a pane capture.
    pub async fn display_message(&self, session: &str, field: DisplayField) -> Result<String> {
        validate_value("session name", session)?;
        let mut args = self.server_args();
        args.extend([
            "display-message".into(),
            "-p".into(),
            "-t".into(),
            session.into(),
            field.format().into(),
        ]);
        Ok(self.invoke(args).await?.stdout.trim_end().to_owned())
    }

    fn server_args(&self) -> Vec<OsString> {
        vec!["-L".into(), SERVER_LABEL.into()]
    }

    async fn invoke(&self, args: Vec<OsString>) -> Result<CommandOutput> {
        let output = self.run(&args).await?;
        if output.success {
            Ok(output)
        } else {
            command_failed(&args, &output)
        }
    }

    async fn run(&self, args: &[OsString]) -> Result<CommandOutput> {
        self.runner
            .run(&self.program, args, None)
            .await
            .with_context(|| format!("spawning tmux executable {:?}", self.program))
    }
}

fn parse_session(line: &str) -> Result<Session> {
    let (name, token) = line
        .split_once('\t')
        .ok_or_else(|| anyhow::anyhow!("unexpected tmux list-sessions row: {line:?}"))?;
    validate_value("session name", name)?;
    Ok(Session {
        name: name.to_owned(),
        spawn_token: (!token.is_empty()).then(|| token.to_owned()),
    })
}

/// True when tmux's stderr indicates the target session (or the whole
/// private server) simply does not exist, as opposed to a real command
/// failure. tmux reports this a few different ways depending on whether the
/// session is missing or the server itself was never started:
/// `"can't find session: <name>"`, `"session not found: <name>"`, and
/// `"no server running on <socket>"`.
fn is_absent_session_stderr(stderr: &str) -> bool {
    stderr.contains("can't find session")
        || stderr.contains("session not found")
        || stderr.contains("no server running")
}

fn command_failed<T>(args: &[OsString], output: &CommandOutput) -> Result<T> {
    bail!(
        "tmux command {:?} exited {:?}: {}",
        args.iter().map(|arg| arg.to_string_lossy()).collect::<Vec<_>>(),
        output.code,
        output.stderr.trim()
    )
}

fn utf8_chunks(text: &str, max_bytes: usize) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + max_bytes).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end > start && &text[end..] == ";" {
            end -= 1;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

#[cfg(test)]
mod tests;
