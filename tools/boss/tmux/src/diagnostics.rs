//! Narrow classification of tmux diagnostics. Unknown failures stay errors.

/// True when tmux's stderr indicates the target session (or the whole
/// private server) simply does not exist, as opposed to a real command
/// failure. tmux reports this a few different ways depending on whether the
/// session is missing, the server exited leaving its socket behind, or the
/// socket file cannot be connected:
/// `"can't find session: <name>"`, `"session not found: <name>"`,
/// `"no such session: <name>"` (tmux 3.6a),
/// `"no server running on <socket>"`, and
/// `"error connecting to <socket> (No such file or directory|Connection refused)"`.
///
/// `ENOENT` is the shape a host reboot produces for the legacy `-L boss`
/// server. macOS clears `/tmp` on boot, so `/tmp/tmux-<uid>/boss` is not just
/// stale but absent, and tmux switches from "no server running" to a
/// `connect(2)` error. Reading that as a hard failure strands the
/// coordinator: every recovery path funnels through
/// [`crate::Tmux::list_sessions`], which would return `Err` instead of an empty
/// inventory, so `coordinator_tmux` never reaches the branch that recreates
/// the session and the pane stays blank until the socket reappears by some
/// other means.
///
/// `Connection refused` is the matching shape for a leftover durable socket
/// file (`<state-root>/tmux.sock`) with no listener. That path survives
/// `/tmp` cleanup, so a dead server leaves a unix socket that `connect(2)`
/// refuses rather than `ENOENT`. [`crate::Tmux::unlink_stale_socket_file`] removes
/// that file at startup; treating the stderr as absent keeps `list_sessions`
/// empty if a command races ahead of the unlink.
///
/// Other `connect(2)` failures — `Permission denied` on a socket owned by
/// another uid — describe a socket that exists and is not ours to assume is
/// empty, and must keep surfacing as real errors.
pub(super) fn is_absent_session_stderr(stderr: &str) -> bool {
    is_diagnostic(stderr, &["can't find session", "session not found", "no such session"])
        || is_absent_server_stderr(stderr)
}

pub(super) fn is_absent_server_stderr(stderr: &str) -> bool {
    let stderr = stderr.trim_end();
    if stderr.contains(['\n', '\r']) {
        return false;
    }
    stderr.starts_with("no server running on ")
        || (stderr.starts_with("error connecting to ")
            && (stderr.ends_with(" (No such file or directory)") || stderr.ends_with(" (Connection refused)")))
}

/// Match the diagnostic itself, never a phrase embedded in a target/path or
/// another error. Mixed diagnostics must fail closed. `has-session` alone is
/// not a substitute: a nonzero exit can also mean an inaccessible server.
pub(super) fn is_diagnostic(stderr: &str, messages: &[&str]) -> bool {
    let stderr = stderr.trim_end();
    !stderr.contains(['\n', '\r'])
        && messages.iter().any(|message| {
            stderr == *message || stderr.strip_prefix(message).is_some_and(|tail| tail.starts_with(": "))
        })
}
