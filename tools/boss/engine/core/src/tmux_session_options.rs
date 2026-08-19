//! Presentation options Boss owns for each of its tmux sessions.
//!
//! These are session-scoped deliberately: Boss must not inherit presentation
//! from a user's tmux configuration or change unrelated tmux sessions.

use anyhow::{Context, Result};
use boss_tmux::Tmux;

const BOSS_SESSION_OPTIONS: &[(&str, &str)] = &[("status", "off")];

/// Apply the session options owned by Boss after tmux has loaded user config.
pub(crate) async fn apply(tmux: &Tmux, session_name: &str) -> Result<()> {
    for &(option, value) in BOSS_SESSION_OPTIONS {
        tmux.set_option(session_name, option, value)
            .await
            .with_context(|| format!("setting Boss tmux session option {option}={value} for {session_name}"))?;
    }
    Ok(())
}
