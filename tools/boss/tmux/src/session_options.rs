//! Options Boss owns on its private (`-L boss`) tmux server and sessions.
//!
//! Session options are set per Boss session so presentation is not inherited
//! from a user's tmux configuration. Server options apply to the whole
//! `-L boss` server, which Boss owns exclusively. Both the hub engine and the
//! spoke daemon create worker sessions through [`apply`], so a viewer sees the
//! same session wherever the worker runs.

use anyhow::{Context, Result};

use crate::{OptionScope, OptionSetting, Tmux, TmuxVersion};

const BOSS_SESSION_OPTIONS: &[(&str, &str)] = &[("status", "off"), ("remain-on-exit", "on")];

/// Server-scoped options required for modified keys (e.g. Ctrl+Enter) to
/// reach the pane, plus `focus-events` so attached clients receive
/// FocusIn/FocusOut. Indexed `terminal-features` assignment is idempotent;
/// `-a` would append a duplicate on every `apply()`. `on` (not `always`)
/// honours apps that request xterm modifyOtherKeys mode 2 themselves.
/// Leave `extended-keys-format` at its default (`xterm`). Escalate to
/// `always` / `csi-u` only if the xterm form is misparsed. `focus-events`
/// is a server option (tmux man page), not a session option.
const BOSS_SERVER_OPTIONS: &[(&str, &str)] = &[
    ("terminal-features[100]", "xterm*:extkeys"),
    ("extended-keys", "on"),
    ("focus-events", "on"),
];

/// Converge Boss-owned tmux options after tmux has loaded user config.
///
/// Server options are set first so `terminal-features` is in place before
/// the caller returns an identity used to attach a client.
pub async fn apply(tmux: &Tmux, session_name: &str) -> Result<()> {
    let mut settings = Vec::new();
    for &(option, value) in BOSS_SERVER_OPTIONS {
        if tmux.show_server_option(option).await? != Some(value.to_owned()) {
            settings.push(OptionSetting {
                scope: OptionScope::Server,
                option,
                value,
            });
        }
    }
    for &(option, value) in BOSS_SESSION_OPTIONS {
        if tmux.show_option(session_name, option).await? != Some(value.to_owned()) {
            settings.push(OptionSetting {
                scope: OptionScope::Session(session_name),
                option,
                value,
            });
        }
    }
    tmux.set_options(&settings)
        .await
        .context("setting Boss tmux options that do not already match")?;
    Ok(())
}

/// Result of [`source_driver_session_config`] when no error occurred.
#[derive(Debug)]
pub enum SessionConfigOutcome {
    Sourced,
    /// tmux predates 3.4's `source-file -t`; nothing was sent.
    UnsupportedTmux(TmuxVersion),
    /// The tmux version could not be read; nothing was sent.
    VersionUnreadable(anyhow::Error),
}

/// Source a driver's session-scoped tmux config (e.g. codex's `mouse on`).
///
/// Sourced with `-t` so it is scoped to this one session, never the whole
/// server. `-t` on `source-file` needs tmux 3.4+ (Boss's floor is 3.2), so the
/// version is probed first: an older server rejects the flag outright, which
/// would otherwise fail the whole spawn over a cosmetic scrollback fix.
pub async fn source_driver_session_config(
    tmux: &Tmux,
    session_name: &str,
    config: &str,
) -> Result<SessionConfigOutcome> {
    match tmux.version().await {
        Ok(version) if version.supports_source_file_target() => {
            tmux.source_file(session_name, config).await?;
            Ok(SessionConfigOutcome::Sourced)
        }
        Ok(version) => Ok(SessionConfigOutcome::UnsupportedTmux(version)),
        Err(error) => Ok(SessionConfigOutcome::VersionUnreadable(error)),
    }
}
