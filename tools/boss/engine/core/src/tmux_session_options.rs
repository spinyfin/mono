//! Options Boss owns on its private (`-L boss`) tmux server and sessions.
//!
//! Session options are set per Boss session so presentation is not inherited
//! from a user's tmux configuration. Server options apply to the whole
//! `-L boss` server, which Boss owns exclusively.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use boss_tmux::{OptionScope, OptionSetting, Tmux};

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

/// Color environment every Boss tmux session receives at creation; see
/// [`boss_tmux::insert_color_environment`].
pub(crate) fn insert_color_environment(environment: &mut BTreeMap<String, String>) {
    boss_tmux::insert_color_environment(environment);
}

/// Start Boss's private server and establish defaults before its first window.
///
/// Bootstrap sends all settings in one tmux command sequence (see
/// [`Tmux::start_private_server`]). [`apply`] repeats `remain-on-exit` per
/// session after user config has loaded.
pub(crate) async fn prepare_server(tmux: &Tmux) -> Result<()> {
    tmux.start_private_server()
        .await
        .context("starting Boss private tmux server before its first window")?;
    crate::tmux_health::server_observed(tmux);
    Ok(())
}

/// Assert a `new-session` argv carries the color `-e` pair and omits `FORCE_COLOR`.
#[cfg(test)]
pub(crate) fn assert_color_environment(new_session: &[String]) {
    assert!(
        new_session
            .windows(2)
            .any(|pair| pair == ["-e", "CLAUDE_CODE_TMUX_TRUECOLOR=1"]),
        "expected CLAUDE_CODE_TMUX_TRUECOLOR=1 at session creation, got {new_session:?}"
    );
    assert!(
        new_session.windows(2).any(|pair| pair == ["-e", "COLORTERM=truecolor"]),
        "expected COLORTERM=truecolor at session creation, got {new_session:?}"
    );
    assert!(
        !new_session.iter().any(|arg| arg.contains("FORCE_COLOR")),
        "FORCE_COLOR must not be set on new-session: {new_session:?}"
    );
}

/// Converge Boss-owned tmux options after tmux has loaded user config.
///
/// Server options are set first so `terminal-features` is in place before
/// the caller returns an identity used to attach a client.
pub(crate) async fn apply(tmux: &Tmux, session_name: &str) -> Result<()> {
    crate::tmux_health::server_observed(tmux);
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

#[cfg(test)]
mod tests {
    use super::insert_color_environment;
    use std::collections::BTreeMap;

    #[test]
    fn color_environment_overrides_ambient_colorterm_and_omits_force_color() {
        let mut environment = BTreeMap::from([("COLORTERM".to_owned(), "something-else".to_owned())]);
        insert_color_environment(&mut environment);
        assert_eq!(
            environment.get("CLAUDE_CODE_TMUX_TRUECOLOR").map(String::as_str),
            Some("1")
        );
        assert_eq!(environment.get("COLORTERM").map(String::as_str), Some("truecolor"));
        assert!(!environment.contains_key("FORCE_COLOR"));
    }
}
