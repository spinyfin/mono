import Darwin
import Foundation

/// Posix kill ladder used to reap the `claude` subprocess behind a
/// libghostty pane when the engine asks the app to release the pane.
///
/// `WorkersWorkspaceModel.releaseWorkerPane` used to drop only the
/// Swift session reference — incident 001 surfaced that the underlying
/// pty's child process (the `claude` worker) kept running indefinitely
/// against the workspace despite the engine's `release_worker_pane`
/// having "succeeded". This helper closes that gap by signalling the
/// foreground process group of the pty before the view tears down, then
/// escalating to `SIGKILL` after a grace window so a `claude` that
/// trapped `SIGTERM` cannot survive the release.
///
/// Mirrors the engine-side `signal_shell_pids` shape (SIGTERM, grace,
/// SIGKILL on whoever's still alive) so the two reap paths behave the
/// same way. Signals are sent to the *process group* of the foreground
/// pid (negated pid argument to `kill(2)`) so descendants of `claude`
/// — e.g. an MCP server it spawned — go too.
///
/// Designed to be unit-testable without a real pty: every syscall has
/// an injectable closure with a libc-backed default. Tests substitute
/// stubs that record calls and skip the real sleep.
enum WorkerProcessKiller {
    /// `kill(2)`. Returns 0 on success, -1 on failure (errno set).
    typealias Signaler = @Sendable (pid_t, Int32) -> Int32
    /// `getpgid(2)`. Returns the process group id, or -1 on failure.
    typealias PgidResolver = @Sendable (pid_t) -> pid_t
    /// `kill(pid, 0)` style liveness probe. True when the pid is still
    /// reachable (running, or alive but owned by another uid).
    typealias AliveCheck = @Sendable (pid_t) -> Bool
    /// Async sleep so tests can run the grace window in zero wall time.
    typealias Sleeper = @Sendable (UInt32) async -> Void

    static let realSignal: Signaler = { pid, sig in Darwin.kill(pid, sig) }
    static let realPgid: PgidResolver = { pid in Darwin.getpgid(pid) }
    static let realIsAlive: AliveCheck = { pid in
        if Darwin.kill(pid, 0) == 0 { return true }
        return errno == EPERM
    }
    static let realSleep: Sleeper = { seconds in
        guard seconds > 0 else { return }
        let nanos = UInt64(seconds) * 1_000_000_000
        try? await Task.sleep(nanoseconds: nanos)
    }

    /// `SIGTERM` the process group of `pid`, wait `graceSeconds`, then
    /// `SIGKILL` if the lead pid is still around. `pid <= 0` is a
    /// no-op so callers can pass "no foreground pid known" without
    /// branching at the call site.
    ///
    /// We signal `-pgid` instead of `pid` so a `claude` that itself
    /// spawned helpers (e.g. an MCP stdio child) takes them with it.
    /// If `getpgid` fails (the process is already gone), fall back to
    /// signalling the lead pid directly — `kill` will return `ESRCH`
    /// and the no-op return path takes care of the rest.
    static func killForegroundProcessTree(
        pid: pid_t,
        graceSeconds: UInt32,
        signal: Signaler = WorkerProcessKiller.realSignal,
        getpgid: PgidResolver = WorkerProcessKiller.realPgid,
        isAlive: AliveCheck = WorkerProcessKiller.realIsAlive,
        sleep: Sleeper = WorkerProcessKiller.realSleep
    ) async {
        guard pid > 0 else { return }

        let target = signalTarget(pid: pid, getpgid: getpgid)
        _ = signal(target, SIGTERM)

        await sleep(graceSeconds)

        guard isAlive(pid) else { return }
        _ = signal(target, SIGKILL)
    }

    /// Resolve the pid argument we pass to `kill(2)`. Prefers the
    /// process group leader (negated, so `kill` signals the whole
    /// group) and falls back to the lead pid when `getpgid` reports
    /// "no such process" or any other failure.
    static func signalTarget(pid: pid_t, getpgid: PgidResolver) -> pid_t {
        let pgid = getpgid(pid)
        return pgid > 0 ? -pgid : pid
    }
}
