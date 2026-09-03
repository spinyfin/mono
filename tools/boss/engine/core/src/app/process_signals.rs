//! Process-liveness and OS-signal helpers used across the app module:
//! constant-time token comparison, pid-ancestry trust checks, and the
//! SIGTERM/SIGKILL reap paths used both by the pane-release flow and by
//! engine shutdown.
//!
//! Split out of `server.rs`; pure structural move — no behavioural change.

use super::*;

/// Constant-time byte comparison. Used by the shutdown-RPC token
/// gate so a wrong-length or wrong-content token can't be inferred
/// from response timing — the same costs as the real comparison,
/// regardless of where the mismatch lands.
pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Walk up `pid`'s process tree (bounded depth) checking whether
/// any ancestor matches one of `trust_roots`. Used to implement
/// `LOCAL_PEERPID` subtree-match auth: a peer running inside a
/// trusted process tree is treated as that tree's tier.
pub(super) fn is_descendant_of_any(pid: libc::pid_t, trust_roots: &[libc::pid_t]) -> bool {
    use crate::worker_registry::parent_pid;
    const TRUST_WALK_DEPTH: usize = 16;
    let mut current = pid;
    for _ in 0..TRUST_WALK_DEPTH {
        if trust_roots.contains(&current) {
            return true;
        }
        match parent_pid(current) {
            Ok(Some(parent)) => current = parent,
            Ok(None) | Err(_) => return false,
        }
    }
    false
}

/// Whether `pid` names a live process. Implemented with `kill(pid, 0)`,
/// which delivers no signal but performs the existence + permission
/// check: `Ok` means the process exists, `EPERM` means it exists but is
/// owned by another user (still alive), and `ESRCH` means no such
/// process. Used by `RegisterAppSession` to decide whether a stale app
/// trust root can be superseded by a relaunched app — only when the old
/// app process is genuinely gone.
pub(super) fn pid_is_alive(pid: libc::pid_t) -> bool {
    // Reject pid <= 0: `kill(0, _)` targets the caller's process group
    // and `kill(-pid, _)` a process group, neither of which is the
    // single-process liveness probe we want — interpreting their result
    // as "alive" would be wrong.
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs no action beyond the
    // existence/permission probe; we only read `errno` on failure.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Decide whether a `RegisterAppSession` from `peer_pid` should be
/// trusted, given the currently-pinned app trust root `current_app_pid`
/// and the engine's own pid. Extracted from the connection handler so
/// the trust transitions (matching pid, engine-ancestor, dead-old-app
/// reattach) are unit-testable. See the call site for the rationale of
/// each branch.
pub(super) fn register_app_session_trust_ok(
    current_app_pid: Option<libc::pid_t>,
    peer_pid: Option<libc::pid_t>,
    engine_pid: libc::pid_t,
) -> bool {
    match (current_app_pid, peer_pid) {
        (None, _) => true, // tests / no-trust-root mode
        (Some(expected), Some(observed)) => {
            observed == expected || is_descendant_of_any(engine_pid, &[observed]) || !pid_is_alive(expected)
        }
        (Some(_), None) => false,
    }
}

/// Resolve the `last_status_actor` string for an RPC-driven status change.
///
/// Returns `"boss"` when the caller's process ancestry matches the registered
/// Boss-coordinator session pid; `"human"` otherwise. Engine-internal writers
/// stamp `"engine"` directly in SQL and never call this function.
pub(super) fn resolve_status_actor(server_state: &ServerState, peer_pid: Option<libc::pid_t>) -> &'static str {
    let boss_pid = server_state.current_boss_pid();
    if let (Some(boss_pid), Some(peer_pid)) = (boss_pid, peer_pid)
        && is_descendant_of_any(peer_pid, &[boss_pid])
    {
        return boss_protocol::LAST_STATUS_ACTOR_BOSS;
    }
    boss_protocol::LAST_STATUS_ACTOR_HUMAN
}

pub(super) fn current_parent_pid() -> Option<libc::pid_t> {
    // BOSS_APP_PID is the only signal we trust to identify the app
    // tier. The macOS app sets it to its own pid before spawning the
    // engine — necessary because `bazel run` daemonizes its server,
    // reparenting the engine away from the app's process tree, so
    // `getppid()` lands on `bazel` (or launchd) instead of the app.
    //
    // When BOSS_APP_PID is unset we leave app_pid as None rather than
    // guessing from `getppid()`. Falling back to the parent yields a
    // wrong-but-confident answer in every dev setup that launches the
    // engine independently of the app (`bazel run` from a terminal,
    // direct invocation of the binary, etc.) — the engine pins its
    // trust root to bazel/launchd and then rejects every legitimate
    // `RegisterAppSession` from the real app, which kills dispatch
    // (every `SpawnWorkerPane` request fails because no app session
    // is registered to receive it). With None, the trust gate becomes
    // a no-op (matches the test path), the app registers, and the
    // coordinator tmux pane becomes the real trust root once created.
    // Production is unaffected: the app always sets BOSS_APP_PID via
    // `EngineProcessController`.
    std::env::var("BOSS_APP_PID")
        .ok()
        .and_then(|raw| raw.parse::<libc::pid_t>().ok())
        .filter(|&pid| pid > 1)
}

/// Send `SIGTERM` to every pid in `pids`, sleep `grace`, then send
/// `SIGKILL` to anything still alive. Used as the shutdown fallback
/// when the app teardown path didn't release the worker shell — and
/// from the panic hook, where we must not touch the runtime. The
/// loop keeps going past `EPERM` / `ESRCH` because the worker may
/// already be dead (good) or owned by another uid (we can't help).
/// Engine-side backstop reap of a worker's OS process tree on pane
/// release. The macOS app's `releaseWorkerPane` (→ `WorkerProcessKiller`)
/// is the primary reaper, but it cannot act when no app session is
/// registered, when the app is unresponsive, or when a wedged surface
/// reports no foreground pid. In those cases `release_worker_pane` used
/// to free the engine slot and the cube lease while the worker's
/// `claude` process kept running — the leak in #975, where `bossctl
/// agents stop` cleared the slot but left the OS process alive.
///
/// Fires `SIGTERM` at the *process group* of `shell_pid` synchronously
/// (so a `claude` and anything it spawned — e.g. an MCP stdio child —
/// go too), then escalates to `SIGKILL` on a detached task after
/// `grace` if the lead pid is still alive. A non-positive pid is a
/// no-op so callers need not branch on "pid not reported yet".
///
/// Synchronous SIGTERM + detached SIGKILL (rather than a blocking
/// ladder) keeps the release path — and the `bossctl agents stop`
/// round-trip behind it — prompt: by the time it returns the worker
/// has at minimum been asked to exit. Mirrors the app-side
/// `WorkerProcessKiller` ladder and the `signal_shell_pids` shutdown
/// fallback.
pub(super) fn reap_worker_process_tree(shell_pid: i32, grace: Duration) {
    if shell_pid <= 0 {
        return;
    }
    let pid = shell_pid as libc::pid_t;
    let target = process_group_signal_target(pid);
    // SAFETY: `pid` was recorded by us at spawn; a failed kill is not
    // fatal (the process may already have exited).
    let rc = unsafe { libc::kill(target, libc::SIGTERM) };
    tracing::debug!(pid, target, rc, "reap_worker_process_tree: SIGTERM");
    tokio::spawn(async move {
        if grace > Duration::from_secs(0) {
            tokio::time::sleep(grace).await;
        }
        if matches!(
            crate::dead_pid_sweep::probe_pid(pid),
            crate::dead_pid_sweep::PidStatus::Dead
        ) {
            tracing::debug!(pid, "reap_worker_process_tree: exited after SIGTERM");
            return;
        }
        // SAFETY: same as above.
        let rc = unsafe { libc::kill(target, libc::SIGKILL) };
        tracing::info!(
            pid,
            target,
            rc,
            "reap_worker_process_tree: process survived SIGTERM grace; escalated to SIGKILL",
        );
    });
}

/// Resolve the `kill(2)` target for `pid`: the negated process group id
/// when `getpgid` succeeds (so the whole group is signalled, reaching
/// descendants), falling back to the bare pid when `getpgid` reports
/// the process is already gone. Mirrors the app-side
/// `WorkerProcessKiller.signalTarget`.
pub(super) fn process_group_signal_target(pid: libc::pid_t) -> libc::pid_t {
    // SAFETY: `getpgid` only reads kernel state for `pid`.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid > 0 { -pgid } else { pid }
}

pub(super) fn signal_shell_pids(pids: &[libc::pid_t], grace: Duration) {
    if pids.is_empty() {
        return;
    }
    for &pid in pids {
        // SAFETY: `kill` with a pid we recorded ourselves; failure is
        // logged but not fatal.
        let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
        if rc != 0 {
            tracing::debug!(
                pid,
                errno = std::io::Error::last_os_error().raw_os_error(),
                "shutdown_workers: SIGTERM returned non-zero (likely already exited)",
            );
        }
    }
    if grace > Duration::from_secs(0) {
        std::thread::sleep(grace);
    }
    for &pid in pids {
        // SAFETY: same as above.
        let rc = unsafe { libc::kill(pid, libc::SIGKILL) };
        if rc != 0 {
            tracing::debug!(
                pid,
                errno = std::io::Error::last_os_error().raw_os_error(),
                "shutdown_workers: SIGKILL returned non-zero",
            );
        }
    }
}
