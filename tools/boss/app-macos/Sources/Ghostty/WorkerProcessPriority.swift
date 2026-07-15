import Darwin
import Foundation

/// Clamps a worker pane's shell — and therefore its whole process tree —
/// to Darwin's background scheduling band, so build/test workloads under
/// full fleet load no longer compete with the coordinator's own UI work
/// on equal terms.
///
/// Root cause (2026-07-15 rescope): under 22 concurrent workers the
/// coordinator pane's typing went sluggish not because of an app bug but
/// because every core was pegged (load average ~152, ~10x oversubscribed)
/// and macOS was scheduling keystroke handling alongside 22 build/test
/// workloads with no priority distinction. The fix is scheduling
/// isolation, not app profiling.
///
/// Mechanism: `setpriority(2)` with `PRIO_DARWIN_PROCESS` on the worker's
/// shell pid. Per `taskpolicy(8)` ("All children of the specified program
/// also inherit these policies"), Darwin's background scheduling state is
/// a proc-wide flag that is inherited by every child a process forks
/// *after* the flag is set — so applying it once to the shell, before any
/// build tooling runs under it, reaches bazel servers, cargo, rustc,
/// node, etc. without touching each of them individually.
///
/// The Boss app itself, the coordinator's own session pane
/// (`BossPaneModel`), and pane rendering never go through
/// `spawnWorkerPane`/`applyBackgroundPriority` and stay at normal/
/// user-interactive scheduling.
enum WorkerProcessPriority {
    /// `UserDefaults` key toggling the clamp; unset means "on" (the
    /// default). Set with e.g. `defaults write com.anthropic.boss
    /// boss.worker.backgroundPriorityEnabled -bool NO` to disable, in case
    /// background QoS proves too aggressive for worker throughput on
    /// Apple Silicon E-cores.
    static let enabledDefaultsKey = "boss.worker.backgroundPriorityEnabled"

    /// The `BOSS_WORKER_BACKGROUND_PRIORITY` env var takes precedence over
    /// the `UserDefaults` toggle so scripted/CI launches can override
    /// without touching user defaults. Any value other than "0"/"false"
    /// (case-insensitive) is treated as enabled.
    static var isEnabled: Bool {
        if let envOverride = ProcessInfo.processInfo.environment["BOSS_WORKER_BACKGROUND_PRIORITY"] {
            return envOverride != "0" && envOverride.lowercased() != "false"
        }
        if UserDefaults.standard.object(forKey: enabledDefaultsKey) != nil {
            return UserDefaults.standard.bool(forKey: enabledDefaultsKey)
        }
        return true
    }

    /// Apply the background scheduling clamp to a worker pane's shell pid.
    /// Best-effort and non-fatal: a failed `setpriority` call only logs —
    /// it must never block or fail a worker spawn.
    static func applyBackgroundPriority(toShellPid pid: pid_t, runId: String) {
        guard isEnabled else { return }
        guard pid > 0 else { return }
        let result = setpriority(PRIO_DARWIN_PROCESS, id_t(pid), PRIO_DARWIN_BG)
        if result != 0 {
            let message = String(cString: strerror(errno))
            NSLog(
                "WorkerProcessPriority: setpriority(PRIO_DARWIN_BG) failed for run %@ pid %d: %@ (errno %d)",
                runId, pid, message, errno
            )
        }
    }
}
