//! Typed control surface for Boss's private tmux server.
//!
//! Every production command is scoped to an explicit socket beneath Boss's
//! durable state root. This avoids tmux's label-based default socket directory
//! under `/tmp`. The process runner is injectable so callers can test command
//! construction without a live tmux daemon.

mod types;

pub use boss_command_runner::{CommandOutput, CommandRunner, RealCommandRunner};
pub use boss_shell_quote::shell_quote as quote_for_shell;
pub use types::{
    DEFAULT_SEND_CHUNK_BYTES, DEFAULT_SEND_CHUNK_DELAY, DisplayField, MINIMUM_VERSION, NewSession, SERVER_LABEL,
    Session, TMUX_SPAWN_TOKEN_ENV, TmuxVersion,
};

use std::ffi::OsString;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Scope for an option set by [`Tmux::start_server_with_options`] or
/// [`Tmux::set_options`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionScope<'a> {
    /// An option owned by the private tmux server.
    Server,
    /// An option owned by one tmux session.
    Session(&'a str),
    /// A global default inherited by future sessions and windows.
    Global,
}

/// One option assignment in a batched [`Tmux::set_options`] invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionSetting<'a> {
    pub scope: OptionScope<'a>,
    pub option: &'a str,
    pub value: &'a str,
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

/// Monotonic id for named paste buffers shared across all [`Tmux`] handles in
/// this process. Multi-line delivery uses a named buffer on both
/// `load-buffer` and `paste-buffer` so concurrent deliveries cannot steal each
/// other's content off tmux's unnamed buffer stack.
static PASTE_BUFFER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Absolute socket path used by command-construction tests. Production
/// callers pass a resolved path from engine config instead of reading
/// the environment here.
pub const TEST_SOCKET_PATH: &str = "/state/boss/tmux.sock";

/// Operator-facing command prefix for a tmux server addressed by a socket path.
///
/// This intentionally does not resolve a tmux binary: callers that only
/// display a socket-addressed command should not need executable discovery.
pub fn operator_prefix_for_socket(path: &Path) -> String {
    format!("tmux -S {}", quote_for_shell(&path.display().to_string()))
}

#[derive(Clone)]
enum ServerAddress {
    /// Pre-move private server addressed with `tmux -L boss`. Production
    /// reaches this only through [`Tmux::for_legacy_label_server`] so a
    /// boot-time drain can still see sessions that survived the socket
    /// move. New sessions never use it.
    Label,
    Socket(PathBuf),
}

/// Handle for one resolved tmux executable and Boss's private server.
#[derive(Clone)]
pub struct Tmux {
    program: PathBuf,
    runner: Arc<dyn CommandRunner>,
    server: ServerAddress,
    /// Memoized result of [`Self::version`]. The resolved executable's
    /// version cannot change under a running server, so callers that probe
    /// it per-spawn (e.g. gating `source-file -t`) don't each fork `tmux -V`.
    version: tokio::sync::OnceCell<TmuxVersion>,
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
    /// Resolves tmux once to a canonical absolute path, scoped to `socket_path`.
    pub fn resolve(socket_path: impl Into<PathBuf>) -> Result<Self> {
        let program = which::which("tmux").context("locating tmux on PATH")?;
        let program =
            std::fs::canonicalize(&program).with_context(|| format!("canonicalizing tmux path {program:?}"))?;
        Self::from_path_with_socket(program, socket_path)
    }

    /// Creates a production controller targeting one explicit private socket.
    pub fn from_path_with_socket(program: impl Into<PathBuf>, socket_path: impl Into<PathBuf>) -> Result<Self> {
        Self::with_runner_and_socket(program, Arc::new(RealCommandRunner), socket_path)
    }

    /// Creates a controller with an explicit socket, for production and
    /// command-construction tests that need to exercise `-S`.
    ///
    /// Refuses in a test process when `socket_path` has production's *shape*
    /// (parent ends `Library/Application Support/Boss`, filename `tmux.sock`)
    /// — computed structurally via [`boss_log_files::is_production_shaped`],
    /// not by comparing `$HOME` (see that function's doc for why: it also
    /// catches a path inherited from a production engine running under a
    /// *different* `$HOME`, or with `HOME` unset here entirely). Unconditional
    /// on the runner, unlike [`Self::for_legacy_label_server_with_runner`]:
    /// no legitimate test ever needs to name this literal path, so there is
    /// no fake-runner-based coverage to preserve by narrowing the check.
    pub fn with_runner_and_socket(
        program: impl Into<PathBuf>,
        runner: Arc<dyn CommandRunner>,
        socket_path: impl Into<PathBuf>,
    ) -> Result<Self> {
        let socket_path = socket_path.into();
        if !socket_path.is_absolute() {
            bail!("tmux socket path must be absolute: {socket_path:?}");
        }
        if boss_log_files::is_test_process()
            && boss_log_files::is_production_shaped(&socket_path, boss_log_files::TMUX_SOCKET_FILENAME)
        {
            bail!(
                "refusing to address the production tmux socket ({}) from a test process — a test must \
                 use a private socket path (e.g. TEST_SOCKET_PATH), never production's",
                socket_path.display()
            );
        }
        Self::with_runner_for_server(program, runner, ServerAddress::Socket(socket_path))
    }

    /// Controller for the pre-move `tmux -L boss` server. Used only by the
    /// boot-time drain that adopts or surfaces sessions that survived the
    /// socket relocation. New sessions must not be created on this handle.
    pub fn for_legacy_label_server(program: impl Into<PathBuf>) -> Result<Self> {
        Self::for_legacy_label_server_with_runner(program, Arc::new(RealCommandRunner))
    }

    /// Test/drain variant of [`Self::for_legacy_label_server`] with an
    /// injectable process runner.
    ///
    /// `-L boss` addresses tmux's default socket directory — unlike
    /// [`Self::with_runner_and_socket`] there is no path parameter to check
    /// the shape of, so a test process cannot be isolated from it by
    /// construction. Refuses whenever `runner.is_real()` — the runner that
    /// could actually reach the live server — leaving every existing test
    /// that injects a fake/stub/scripted runner to exercise legacy-vs-socket
    /// server *selection* logic exactly as before: a fake runner can never
    /// reach a real server no matter what it is pointed at.
    pub fn for_legacy_label_server_with_runner(
        program: impl Into<PathBuf>,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Self> {
        if boss_log_files::is_test_process() && runner.is_real() {
            bail!(
                "refusing to construct a legacy `-L boss` tmux handle with the real command runner from a \
                 test process — `-L boss` addresses tmux's shared default socket directory and cannot be \
                 scoped to a private path; inject a fake CommandRunner instead"
            );
        }
        Self::with_runner_for_server(program, runner, ServerAddress::Label)
    }

    fn with_runner_for_server(
        program: impl Into<PathBuf>,
        runner: Arc<dyn CommandRunner>,
        server: ServerAddress,
    ) -> Result<Self> {
        let program = program.into();
        if !program.is_absolute() {
            bail!("tmux executable path must be absolute: {program:?}");
        }
        Ok(Self {
            program,
            runner,
            server,
            version: tokio::sync::OnceCell::new(),
        })
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Process runner this handle uses. Callers that need a second handle
    /// against a different server (the pre-move `-L boss` server) pass this
    /// to [`Self::for_legacy_label_server_with_runner`] so a stubbed socket
    /// handle stays stubbed instead of swapping in [`RealCommandRunner`].
    pub fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.runner)
    }

    /// Socket this handle addresses. `None` for a legacy `-L boss` handle.
    pub fn socket_path(&self) -> Option<&Path> {
        match &self.server {
            ServerAddress::Socket(path) => Some(path),
            ServerAddress::Label => None,
        }
    }

    /// Durable server identity recorded on `work_runs.tmux_server_label`:
    /// the absolute socket path, or the literal [`SERVER_LABEL`] for a
    /// session that still lives on the pre-move `-L boss` server.
    pub fn server_identity(&self) -> String {
        match &self.server {
            ServerAddress::Socket(path) => path.display().to_string(),
            ServerAddress::Label => SERVER_LABEL.to_owned(),
        }
    }

    /// Operator-facing command prefix (`tmux -S <socket>` or `tmux -L boss`).
    ///
    /// The addressing flags come from the same server argv spawn uses,
    /// with the value POSIX-quoted for paste.
    pub fn operator_prefix(&self) -> String {
        match &self.server {
            ServerAddress::Label => format!("tmux {}", self.server_shell_args()),
            ServerAddress::Socket(path) => operator_prefix_for_socket(path),
        }
    }

    /// If this handle targets a socket file that exists but no server is
    /// listening, unlink it so later commands can start a fresh server.
    /// No-op for a label-addressed handle, a missing file, or a live server.
    pub fn unlink_stale_socket_file(&self) -> Result<bool> {
        let Some(path) = self.socket_path() else {
            return Ok(false);
        };
        unlink_stale_unix_socket(path)
    }

    /// Operator-facing attach command for an already-verified session.
    ///
    /// Uses this handle's resolved executable and the same server
    /// addressing spawn uses. No `exec` prefix:
    /// this is meant to be pasted into an existing shell, so detaching
    /// from the session should return to a prompt. The program, socket
    /// (or label), and session name are POSIX-quoted so a path with
    /// spaces (`Application Support`) pastes as one token.
    pub fn attach_session_command(&self, session_name: &str) -> String {
        format!(
            "{} {} attach-session -t {}",
            quote_for_shell(&self.program.display().to_string()),
            self.server_shell_args(),
            quote_for_shell(session_name),
        )
    }

    /// Probes the resolved executable's version before the engine accepts work.
    ///
    /// Memoized: the version of a resolved executable cannot change while
    /// this handle is in use, so only the first call forks `tmux -V`.
    pub async fn version(&self) -> Result<TmuxVersion> {
        self.version
            .get_or_try_init(|| async {
                let mut args = self.server_args();
                args.push("-V".into());
                let output = self.invoke(args).await?;
                TmuxVersion::parse(&output.stdout)
            })
            .await
            .copied()
    }

    /// Starts the private server and applies settings before any window exists.
    ///
    /// tmux shuts down an empty server by default. Sending `start-server` and
    /// the initial settings in one command sequence lets the caller set
    /// `exit-empty=off` before the client disconnects, while global defaults
    /// such as `history-limit` still take effect for the first window.
    /// Session-scoped settings are rejected because there is no session yet.
    pub async fn start_server_with_options(&self, settings: &[OptionSetting<'_>]) -> Result<()> {
        let mut args = self.server_args();
        args.push("start-server".into());
        push_option_commands(&mut args, settings, true)?;
        self.invoke(args).await.map(|_| ())
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

    /// Sources tmux commands from `content` into `session`'s scope, as
    /// `source-file -t <session> -` with `content` piped over stdin (`-`
    /// reads the file from stdin — no temp file needed, mirroring how
    /// [`Self::send_keys`] streams multi-line text through `load-buffer`).
    ///
    /// A `set` command in `content` that omits `-g` resolves against the
    /// `-t` target, so this applies session-scoped options to `session`
    /// alone rather than the whole server. A `set -g` in `content` still
    /// means "every session on the server", regardless of `-t` — callers
    /// that need session scoping must omit `-g` in the sourced content.
    pub async fn source_file(&self, session: &str, content: &str) -> Result<()> {
        validate_value("session name", session)?;
        let mut args = self.server_args();
        args.extend(["source-file".into(), "-t".into(), session.into(), "-".into()]);
        self.invoke_with_stdin(args, content.as_bytes()).await.map(|_| ())
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

    /// Applies several option assignments in one tmux command sequence.
    ///
    /// The individual `set-option` commands retain tmux's normal scopes, but
    /// using its `\;` separator avoids a process launch for every assignment.
    pub async fn set_options(&self, settings: &[OptionSetting<'_>]) -> Result<()> {
        if settings.is_empty() {
            return Ok(());
        }
        let mut args = self.server_args();
        push_option_commands(&mut args, settings, false)?;
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
                // The session can still die on its own between the token
                // read above and this call — the ordinary worker-completion
                // race. Treat a kill that fails because the session is
                // already gone as Absent, not a hard error, so this window
                // doesn't leak the identity columns the way an unclassified
                // Refused would.
                if self.kill_session_unchecked(session).await? {
                    Ok(KillSessionOutcome::Killed)
                } else {
                    Ok(KillSessionOutcome::Absent)
                }
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
    /// teardown entry point, by design — see its doc comment. Returns
    /// `Ok(true)` when the session was actually killed, `Ok(false)` when it
    /// had already vanished (a real command failure still becomes `Err`).
    async fn kill_session_unchecked(&self, session: &str) -> Result<bool> {
        validate_value("session name", session)?;
        let mut args = self.server_args();
        args.extend(["kill-session".into(), "-t".into(), session.into()]);
        let output = self.run(&args).await?;
        if output.success {
            return Ok(true);
        }
        if is_absent_session_stderr(&output.stderr) {
            return Ok(false);
        }
        command_failed(&args, &output)
    }

    /// Destroys the entire private tmux server, tearing down every session
    /// on it. This is for test teardown of a server a fixture started
    /// itself (e.g. via [`Self::start_server_with_options`]); production
    /// code destroys only its own session, through
    /// [`Self::kill_session_verified`], and never the shared server.
    ///
    /// A server that is already gone is treated as success, so repeated
    /// teardown calls stay idempotent.
    pub async fn kill_server(&self) -> Result<()> {
        let mut args = self.server_args();
        args.push("kill-server".into());
        let output = self.run(&args).await?;
        if output.success || is_absent_session_stderr(&output.stderr) {
            return Ok(());
        }
        command_failed(&args, &output)
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
        let (flag, value) = self.server_arg_pair();
        vec![flag.into(), value]
    }

    /// Spawn's server argv, formatted for a human to paste into a shell.
    ///
    /// One source of truth: the flag and value are `server_arg_pair`;
    /// only the value is quoted.
    fn server_shell_args(&self) -> String {
        let (flag, value) = self.server_arg_pair();
        format!("{flag} {}", quote_for_shell(&value.to_string_lossy()))
    }

    fn server_arg_pair(&self) -> (&'static str, OsString) {
        match &self.server {
            ServerAddress::Label => ("-L", SERVER_LABEL.into()),
            ServerAddress::Socket(path) => ("-S", path.clone().into_os_string()),
        }
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

/// Appends `set-option` commands for `settings` onto an in-progress argv,
/// shared by [`Tmux::start_server_with_options`] and [`Tmux::set_options`].
///
/// `leading_separator` selects the two entry points' differing separator
/// placement: `start_server_with_options` follows a leading `start-server`
/// and needs a `;` before every setting, while `set_options` needs one only
/// between settings. `leading_separator` also marks the "starting an empty
/// server" context, where a session-scoped option is rejected outright —
/// there is no session yet for `-t` to address.
fn push_option_commands(
    args: &mut Vec<OsString>,
    settings: &[OptionSetting<'_>],
    leading_separator: bool,
) -> Result<()> {
    for (index, setting) in settings.iter().enumerate() {
        validate_value("option name", setting.option)?;
        validate_value("option value", setting.value)?;
        if leading_separator || index != 0 {
            args.push(";".into());
        }
        args.push("set-option".into());
        match setting.scope {
            OptionScope::Server => args.push("-s".into()),
            OptionScope::Session(session) => {
                if leading_separator {
                    bail!("cannot set a session option while starting an empty tmux server");
                }
                validate_value("session name", session)?;
                args.extend(["-t".into(), session.into()]);
            }
            OptionScope::Global => args.push("-g".into()),
        }
        args.extend([setting.option.into(), setting.value.into()]);
    }
    Ok(())
}

fn parse_session(line: &str) -> Result<Session> {
    let (name, token) = line
        .split_once('\t')
        .ok_or_else(|| unparseable_session_row_error(line))?;
    validate_value("session name", name)?;
    Ok(Session {
        name: name.to_owned(),
        spawn_token: (!token.is_empty()).then(|| token.to_owned()),
    })
}

/// Diagnose a `list-sessions` row that carries no TAB delimiter.
///
/// The overwhelmingly likely cause is that the calling process had no UTF-8
/// locale: tmux's `server_client_print()` runs output through
/// `utf8_sanitize()` for any client it does not consider UTF-8 capable, and
/// that rewrites every non-printable byte — the TAB delimiter included — to
/// `_`. Naming that here turns an otherwise baffling parse failure into a
/// self-diagnosing one, because the sanitized row is indistinguishable from a
/// session whose name simply contains an underscore.
///
/// `boss_command_runner::RealCommandRunner` forces `LC_CTYPE=UTF-8` precisely
/// so this cannot happen in production; seeing it means some other spawner
/// reached tmux without a locale.
fn unparseable_session_row_error(line: &str) -> anyhow::Error {
    if line.contains('_') && !line.contains('\t') {
        return anyhow::anyhow!(
            "unexpected tmux list-sessions row: {line:?} — the TAB delimiter is missing and the row \
             contains '_', which is how tmux's utf8_sanitize() rewrites non-printable bytes for a \
             client with no UTF-8 locale. Check that LC_CTYPE/LC_ALL/LANG reach the tmux process."
        );
    }
    anyhow::anyhow!("unexpected tmux list-sessions row: {line:?}")
}

/// True when tmux's stderr indicates the target session (or the whole
/// private server) simply does not exist, as opposed to a real command
/// failure. tmux reports this a few different ways depending on whether the
/// session is missing, the server exited leaving its socket behind, or the
/// socket file cannot be connected:
/// `"can't find session: <name>"`, `"session not found: <name>"`,
/// `"no server running on <socket>"`, and
/// `"error connecting to <socket> (No such file or directory|Connection refused)"`.
///
/// `ENOENT` is the shape a host reboot produces for the legacy `-L boss`
/// server. macOS clears `/tmp` on boot, so `/tmp/tmux-<uid>/boss` is not just
/// stale but absent, and tmux switches from "no server running" to a
/// `connect(2)` error. Reading that as a hard failure strands the
/// coordinator: every recovery path funnels through
/// [`Tmux::list_sessions`], which would return `Err` instead of an empty
/// inventory, so `coordinator_tmux` never reaches the branch that recreates
/// the session and the pane stays blank until the socket reappears by some
/// other means.
///
/// `Connection refused` is the matching shape for a leftover durable socket
/// file (`<state-root>/tmux.sock`) with no listener. That path survives
/// `/tmp` cleanup, so a dead server leaves a unix socket that `connect(2)`
/// refuses rather than `ENOENT`. [`Tmux::unlink_stale_socket_file`] removes
/// that file at startup; treating the stderr as absent keeps `list_sessions`
/// empty if a command races ahead of the unlink.
///
/// Other `connect(2)` failures — `Permission denied` on a socket owned by
/// another uid — describe a socket that exists and is not ours to assume is
/// empty, and must keep surfacing as real errors.
fn is_absent_session_stderr(stderr: &str) -> bool {
    stderr.contains("can't find session")
        || stderr.contains("session not found")
        || stderr.contains("no server running")
        || is_absent_socket_stderr(stderr)
}

/// True for tmux's `connect(2)` failure against a socket that is gone or has
/// no listener. See [`is_absent_session_stderr`] for why these are "absent"
/// rather than "failed".
fn is_absent_socket_stderr(stderr: &str) -> bool {
    stderr.contains("error connecting to")
        && (stderr.contains("No such file or directory") || stderr.contains("Connection refused"))
}

/// Unlink `path` when it exists as a unix socket with no listener. A live
/// server, a missing file, a non-socket file, or any other connect error is
/// left alone.
///
/// Linux `connect(2)` returns `ECONNREFUSED` for a regular file as well as
/// for a socket with no listener, so the file-type check is load-bearing:
/// matching on the connect error alone would delete non-socket files.
fn unlink_stale_unix_socket(path: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| format!("stat tmux socket {}", path.display()));
        }
    };
    if !metadata.file_type().is_socket() {
        return Ok(false);
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => Ok(false),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            std::fs::remove_file(path).with_context(|| format!("unlinking stale tmux socket {}", path.display()))?;
            Ok(true)
        }
        Err(_) => Ok(false),
    }
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
