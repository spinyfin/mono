//! Resolution and isolation gating of the engine's state paths at startup,
//! split out of `server.rs` so that file stays under the 3000-line size check.

use super::*;

/// Resolve the pid, events-socket, db, control-token, tmux-socket, and
/// diagnostics-directory paths this start will actually use, and run the
/// isolation gate against them.
///
/// A pure function of its arguments — no env reads, no I/O — pulled out of
/// `run_server` so the wiring that connects [`IsolationPaths`] to a real
/// start is unit-testable — deleting the `ensure_isolated` call below must
/// fail a test, not just the derivation's own tests.
///
/// `pid_env_override`, `token_env_override` and `diagnostics_env_override`
/// are the callers' env-sourced fallbacks (`$BOSS_PID_PATH`,
/// `crate::engine_control::default_token_path()`,
/// `$BOSS_ENGINE_DIAGNOSTICS_DIR`) — passed in rather than read here so this
/// function has no environment dependency of its own.
pub(super) fn resolve_engine_paths(
    isolation: &IsolationPaths,
    cfg: &RuntimeConfig,
    pid_env_override: Option<std::path::PathBuf>,
    token_env_override: Option<std::path::PathBuf>,
    diagnostics_env_override: Option<std::path::PathBuf>,
) -> Result<crate::app::isolation::EnginePaths> {
    // Use the isolation-derived pid path, falling back to env / hard default.
    let pid_file_path = isolation
        .derived
        .pid
        .clone()
        .or(pid_env_override)
        .or_else(default_pid_path)
        .context("HOME must be set to derive the default pid-file path")?;

    // The events socket was resolved into the config in `run` (isolation-derived
    // when this is a fixture, `$BOSS_EVENTS_SOCKET` / the production default
    // otherwise). Nothing here re-reads the environment.
    let Some(events_socket_path) = cfg.work.events_socket_path.clone() else {
        bail!("HOME must be set to derive the default events socket path");
    };

    // Resolved through the isolation guard so a fixture mints its own token
    // instead of overwriting — and, via `ControlTokenGuard::drop`, deleting —
    // production's.
    let control_token_path = isolation.derived.control_token.clone().or(token_env_override);

    let resolved = crate::app::isolation::EnginePaths {
        db: Some(cfg.work.db_path.clone()),
        events_socket: Some(events_socket_path),
        pid: Some(pid_file_path),
        control_token: control_token_path,
        tmux_socket: Some(cfg.work.resolved_tmux_socket_path()),
        // Fixture-scoped unless a private override won; the process-wide
        // population-timing logger is installed from this value, not from a
        // lazy env / `$HOME` read.
        diagnostics_dir: isolation.resolve_diagnostics_dir(diagnostics_env_override),
    };

    // Hard gate, before anything is bound, written, or unlinked: a fixture
    // whose resolved paths still land on production refuses to start. The
    // derivation above is what makes the ordinary case work; this is what
    // makes a miss loud instead of a 50-minute production outage.
    isolation.ensure_isolated(&resolved)?;

    Ok(resolved)
}
