//! The one-time "engine starting (launch environment)" record.
//!
//! Split out of `app.rs` (which sits at the file-size limit) — pure
//! structural move, no behavioural change. Called once from the
//! `ServerState` constructor, right after the build-identity line.

/// Record how this engine was started and what environment it inherited.
///
/// Both facts are load-bearing and neither was recoverable after the fact
/// before this existed. A Finder/Dock launch hands the app launchd's
/// environment, which contains no `LANG`/`LC_*` at all; an engine the app
/// spawns inherits that, and tmux then treats it as a non-UTF-8 client and
/// sanitizes non-printable bytes — including the TAB delimiter
/// `boss_tmux::list_sessions` parses — out of everything it prints. An
/// engine started instead by the CLI's transparent autostart inherits a
/// shell's environment and has a locale, and the app adopts it rather than
/// spawning its own. The two therefore behave differently in a way that is
/// invisible from the outside.
///
/// Incident 006 had to establish which case applied by elimination, from a
/// statistical argument about how often an unrelated parse failed, because
/// nothing logged either fact. See `tools/boss/docs/postmortems/`.
pub(super) fn log_launch_environment() {
    let locale = boss_command_runner::LocaleDiagnostics::probe();
    // `BOSS_APP_PID` is set by the macOS app when it spawns the engine and
    // by nothing else, so its presence is the launch-path discriminator —
    // the same signal `app_pid_from_env` trusts to pin the app trust root.
    let app_spawned = std::env::var_os("BOSS_APP_PID").is_some();
    tracing::info!(
        launched_by = if app_spawned { "app" } else { "standalone" },
        locale_inherited = %locale.inherited_summary(),
        locale_has_utf8 = locale.has_utf8_locale,
        locale_forced = locale.forced.map(|(name, value)| format!("{name}={value}")),
        "engine starting (launch environment)",
    );
    if !locale.has_utf8_locale {
        tracing::warn!(
            locale_inherited = %locale.inherited_summary(),
            "engine inherited no UTF-8 locale; forcing LC_CTYPE=UTF-8 for child processes so tmux \
             does not sanitize non-printable bytes out of its output",
        );
    }
}
