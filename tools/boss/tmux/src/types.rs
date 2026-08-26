use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};

/// The private tmux server used exclusively by Boss.
pub const SERVER_LABEL: &str = "boss";
/// Environment variable carrying the durable identity token for a Boss worker pane.
pub const TMUX_SPAWN_TOKEN_ENV: &str = "BOSS_SPAWN_TOKEN";
/// tmux 3.2 introduced `new-session -e`, required for atomic token carriage.
pub const MINIMUM_VERSION: TmuxVersion = TmuxVersion { major: 3, minor: 2 };
/// A conservative literal-input size that remains below command-line limits.
pub const DEFAULT_SEND_CHUNK_BYTES: usize = 900;
/// Delay between literal chunks, giving the pane's reader time to drain input so long pastes are not truncated.
pub const DEFAULT_SEND_CHUNK_DELAY: Duration = Duration::from_millis(30);

/// A tmux version's numeric compatibility components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TmuxVersion {
    pub major: u16,
    pub minor: u16,
}

impl TmuxVersion {
    /// Parses tmux's `-V` output, such as `tmux 3.6a`.
    pub fn parse(output: &str) -> Result<Self> {
        let version = output
            .trim()
            .strip_prefix("tmux ")
            .ok_or_else(|| anyhow::anyhow!("unexpected tmux version output: {output:?}"))?;
        let mut components = version.splitn(3, '.');
        let major = components
            .next()
            .ok_or_else(|| anyhow::anyhow!("tmux version is missing a major component: {output:?}"))?
            .parse()
            .map_err(|error| anyhow::anyhow!("invalid tmux major version in {output:?}: {error}"))?;
        let minor_text = components
            .next()
            .ok_or_else(|| anyhow::anyhow!("tmux version is missing a minor component: {output:?}"))?;
        let minor_digits: String = minor_text.chars().take_while(char::is_ascii_digit).collect();
        if minor_digits.is_empty() {
            bail!("invalid tmux minor version in {output:?}");
        }
        let minor = minor_digits
            .parse()
            .map_err(|error| anyhow::anyhow!("invalid tmux minor version in {output:?}: {error}"))?;
        Ok(Self { major, minor })
    }

    pub fn supports_session_environment(self) -> bool {
        self >= MINIMUM_VERSION
    }
}

/// One detached tmux session to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub name: String,
    pub environment: BTreeMap<String, String>,
    pub working_directory: PathBuf,
    /// One shell command, as accepted by tmux's `new-session` command tail.
    pub command: String,
}

impl NewSession {
    pub fn validate(&self) -> Result<()> {
        validate_value("session name", &self.name)?;
        validate_value("session command", &self.command)?;
        if !self.working_directory.is_absolute() {
            bail!(
                "tmux session working directory must be absolute: {:?}",
                self.working_directory
            );
        }
        for (name, value) in &self.environment {
            validate_value("environment name", name)?;
            validate_value("environment value", value)?;
            if name.contains('=') {
                bail!("tmux environment name cannot contain '=': {name:?}");
            }
        }
        Ok(())
    }
}

/// Lightweight session record returned by [`crate::Tmux::list_sessions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    /// Convenience mirror `@boss_spawn_token`; the environment remains authoritative.
    pub spawn_token: Option<String>,
}

/// One client attached to Boss's private tmux server, as reported by
/// `list-clients`.
///
/// The two timestamps a wedge diagnosis needs sit on opposite sides of the
/// protocol and mean different things — see
/// `tools/boss/docs/tmux-client-input-wedge.md`:
///
/// - [`Self::activity_epoch`] (`#{client_activity}`) advances **only when
///   this client sends something to the server**, i.e. on input. Rendering
///   output to the client does not touch it, and neither does a
///   server-driven `refresh-client`.
/// - `#{window_activity}` (via [`crate::DisplayField::WindowActivity`])
///   advances on **pane output**, with or without any client attached.
///
/// So a frozen `activity_epoch` alone says nothing: it is equally the
/// signature of a healthy client whose operator is not typing. It only
/// becomes evidence when paired with an independent report that input WAS
/// delivered into that client's pty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxClient {
    /// Controlling terminal of the client, e.g. `/dev/ttys002`. This is
    /// what `detach-client -t` addresses.
    pub tty: String,
    /// Pid of the process that ran `attach-session`. Boss's viewers
    /// `exec` tmux from their launch shell, so this equals the pid the
    /// app reads back from `ghostty_surface_foreground_pid` — which is
    /// how a wedged viewer is told apart from an operator's terminal
    /// attached to the same session.
    pub pid: i32,
    /// Unix seconds of this client's last input to the server.
    pub activity_epoch: i64,
}

/// Fields that can be read without parsing a pane capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayField {
    PanePid,
    PaneDead,
    PaneDeadStatus,
    PaneCurrentCommand,
    WindowActivity,
}

impl DisplayField {
    pub(crate) const fn format(self) -> &'static str {
        match self {
            Self::PanePid => "#{pane_pid}",
            Self::PaneDead => "#{pane_dead}",
            Self::PaneDeadStatus => "#{pane_dead_status}",
            Self::PaneCurrentCommand => "#{pane_current_command}",
            Self::WindowActivity => "#{window_activity}",
        }
    }
}

pub(crate) fn validate_value(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("tmux {label} cannot be empty");
    }
    if value.contains('\0') {
        bail!("tmux {label} cannot contain NUL");
    }
    Ok(())
}
