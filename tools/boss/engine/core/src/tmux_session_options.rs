//! Options Boss owns on its private (`-L boss`) tmux server and sessions.
//!
//! Session options are set per Boss session so presentation is not inherited
//! from a user's tmux configuration. Server options apply to the whole
//! `-L boss` server, which Boss owns exclusively.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use boss_tmux::Tmux;

const BOSS_SESSION_OPTIONS: &[(&str, &str)] = &[("status", "off")];

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

/// Color environment every Boss tmux session receives at creation.
///
/// Claude Code clamps chalk to 256-color whenever `$TMUX` is set, ignoring
/// `COLORTERM` and `FORCE_COLOR`. `CLAUDE_CODE_TMUX_TRUECOLOR` is the escape
/// hatch and is read at module load, so it must be present at process launch.
/// `COLORTERM=truecolor` is explicit rather than inherited because tmux 3.6a
/// injects it, while the installer supports tmux 3.2 and later. `FORCE_COLOR`
/// is deliberately omitted because the clamp overrides it and some values
/// disable a separate TERM-allowlist truecolor path.
pub(crate) fn insert_color_environment(environment: &mut BTreeMap<String, String>) {
    environment.insert("CLAUDE_CODE_TMUX_TRUECOLOR".to_owned(), "1".to_owned());
    environment.insert("COLORTERM".to_owned(), "truecolor".to_owned());
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
