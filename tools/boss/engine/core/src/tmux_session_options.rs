//! Hub-side wiring for the options Boss owns on its private (`-L boss`) tmux
//! server and sessions. The option sets themselves live in
//! [`boss_tmux::session_options`], shared with the spoke daemon.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use boss_tmux::Tmux;

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
    boss_tmux::session_options::apply(tmux, session_name).await
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
