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
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use tokio::time::sleep;

use crate::types::validate_value;

/// Monotonic id for named paste buffers shared across all [`Tmux`] handles in
/// this process. Multi-line delivery uses a named buffer on both
/// `load-buffer` and `paste-buffer` so concurrent deliveries cannot steal each
/// other's content off tmux's unnamed buffer stack.
static PASTE_BUFFER_SEQ: AtomicU64 = AtomicU64::new(0);

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
            if output.stderr.contains("no server running") {
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
        if output.stderr.contains("unknown variable") || output.stderr.contains("environment variable not found") {
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

    /// Sets a server-scoped tmux option, such as `@boss_engine_owner`.
    /// Server options are not addressed to any session — there is no `-t`.
    pub async fn set_server_option(&self, option: &str, value: &str) -> Result<()> {
        validate_value("option name", option)?;
        validate_value("option value", value)?;
        let mut args = self.server_args();
        args.extend(["set-option".into(), "-s".into(), option.into(), value.into()]);
        self.invoke(args).await.map(|_| ())
    }

    /// Reads one server-scoped tmux option, returning `None` when it is unset.
    pub async fn show_server_option(&self, option: &str) -> Result<Option<String>> {
        validate_value("option name", option)?;
        let mut args = self.server_args();
        args.extend(["show-options".into(), "-s".into(), "-v".into(), option.into()]);
        let output = self.run(&args).await?;
        if output.success {
            return Ok(Some(output.stdout.trim_end().to_owned()));
        }
        if output.stderr.contains("invalid option") || output.stderr.contains("unknown option") {
            return Ok(None);
        }
        command_failed(&args, &output)
    }

    /// Submits text as the app transport does: trailing line endings are
    /// removed, multi-line text is bracketed-pasted, and exactly one Return
    /// follows. Single-line text keeps the bounded literal-key chunking.
    pub async fn send_keys(&self, session: &str, text: &str) -> Result<()> {
        validate_value("session name", session)?;
        if text.contains('\0') {
            bail!("tmux key input cannot contain NUL");
        }
        let text = text.trim_end_matches(['\r', '\n']);
        if text.contains(['\r', '\n']) {
            let buffer_name = format!(
                "boss-deliver-{session}-{}",
                PASTE_BUFFER_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            validate_value("buffer name", &buffer_name)?;
            let mut args = self.server_args();
            args.extend([
                "load-buffer".into(),
                "-b".into(),
                buffer_name.clone().into(),
                "-".into(),
            ]);
            self.invoke_with_stdin(args, text.as_bytes()).await?;
            let mut args = self.server_args();
            args.extend([
                "paste-buffer".into(),
                "-b".into(),
                buffer_name.into(),
                "-p".into(),
                "-d".into(),
                "-t".into(),
                session.into(),
            ]);
            self.invoke(args).await?;
        } else {
            self.send_literal_chunks(session, text).await?;
        }
        self.send_key(session, "C-m").await
    }

    async fn send_literal_chunks(&self, session: &str, text: &str) -> Result<()> {
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
        Ok(())
    }

    /// Sends one named tmux key without submitting text. This is for control
    /// input such as `Escape`; callers that need a text prompt should use
    /// [`Self::send_keys`], which deliberately follows the literal text with
    /// a separate Return keypress.
    pub async fn send_key(&self, session: &str, key: &str) -> Result<()> {
        validate_value("session name", session)?;
        validate_value("tmux key", key)?;
        let mut args = self.server_args();
        args.extend(["send-keys".into(), "-t".into(), session.into(), key.into()]);
        self.invoke(args).await.map(|_| ())
    }

    /// Captures the visible text of a detached session's pane.
    pub async fn capture_pane(&self, session: &str) -> Result<String> {
        validate_value("session name", session)?;
        let mut args = self.server_args();
        args.extend(["capture-pane".into(), "-p".into(), "-t".into(), session.into()]);
        Ok(self.invoke(args).await?.stdout)
    }

    /// Destroys exactly one named session on Boss's private server.
    pub async fn kill_session(&self, session: &str) -> Result<()> {
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

    async fn invoke_with_stdin(&self, args: Vec<OsString>, stdin: &[u8]) -> Result<CommandOutput> {
        let output = self
            .runner
            .run_with_stdin(&self.program, &args, None, stdin)
            .await
            .with_context(|| format!("spawning tmux executable {:?}", self.program))?;
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
