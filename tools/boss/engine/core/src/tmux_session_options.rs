//! Options Boss owns on its private (`-L boss`) tmux server and sessions.
//!
//! Session options are set per Boss session so presentation is not inherited
//! from a user's tmux configuration. Server options apply to the whole
//! `-L boss` server, which Boss owns exclusively.

use anyhow::{Context, Result};
use boss_tmux::Tmux;

const BOSS_SESSION_OPTIONS: &[(&str, &str)] = &[("status", "off")];

/// Server-scoped options required for modified keys (e.g. Ctrl+Enter) to
/// reach the pane. Indexed `terminal-features` assignment is idempotent;
/// `-a` would append a duplicate on every `apply()`. `on` (not `always`)
/// honours apps that request xterm modifyOtherKeys mode 2 themselves.
/// Leave `extended-keys-format` at its default (`xterm`). Escalate to
/// `always` / `csi-u` only if the xterm form is misparsed.
const BOSS_SERVER_OPTIONS: &[(&str, &str)] = &[("terminal-features[100]", "xterm*:extkeys"), ("extended-keys", "on")];

/// Apply Boss-owned tmux options after tmux has loaded user config.
///
/// Server options are set first so `terminal-features` is in place before
/// the caller returns an identity used to attach a client.
pub(crate) async fn apply(tmux: &Tmux, session_name: &str) -> Result<()> {
    for &(option, value) in BOSS_SERVER_OPTIONS {
        tmux.set_server_option(option, value)
            .await
            .with_context(|| format!("setting Boss tmux server option {option}={value}"))?;
    }
    for &(option, value) in BOSS_SESSION_OPTIONS {
        tmux.set_option(session_name, option, value)
            .await
            .with_context(|| format!("setting Boss tmux session option {option}={value} for {session_name}"))?;
    }
    Ok(())
}
