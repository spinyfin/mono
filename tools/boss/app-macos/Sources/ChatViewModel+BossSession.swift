import Foundation

/// Session-registration handshake with the engine: the app session, the Boss
/// pane's shell pid, and the per-worker-pane pid/liveness reports ContentView
/// pushes down as libghostty surfaces come and go. The backing state
/// (`isAppSessionRegistered`, `bossPaneShellPidProvider`) lives in
/// `ChatViewModel.swift`; the `.appSessionRegistered` event that flips it
/// arrives via `ChatViewModel+EventHandling.swift`.
extension ChatViewModel {
    /// Called by ContentView when the Boss pane's libghostty surface attaches
    /// (initial creation or after a restart). Sends RegisterBossSession if the
    /// app session is already confirmed; otherwise the registration fires when
    /// appSessionRegistered arrives.
    func bossPaneShellPidAvailable() {
        maybeRegisterBossSession()
    }

    func maybeRegisterBossSession() {
        guard isAppSessionRegistered else { return }
        guard let pid = bossPaneShellPidProvider?(), pid > 0 else { return }
        engine.sendRegisterBossSession(shellPid: pid)
    }

    /// Called by ContentView when a worker pane's libghostty surface attaches
    /// and the shell pid becomes available. Forwards the real pid to the engine
    /// so process tracking, dead-pid sweep, and `bossctl agents stop` work for
    /// reviewer and other shell_pid-0 spawns.
    func workerPaneShellPidAvailable(runId: String, shellPid: Int32) {
        guard isAppSessionRegistered else { return }
        engine.sendUpdateWorkerShellPid(runId: runId, shellPid: shellPid)
    }

    /// Called by ContentView when a worker pane's surface fails to attach
    /// or its child process exits. Reports the death to the engine so it
    /// can reap the backing execution immediately rather than waiting for
    /// the next dead-pid sweep pass or an app restart.
    func workerPaneDied(runId: String) {
        guard isAppSessionRegistered else { return }
        engine.sendWorkerPaneDied(runId: runId)
    }

    /// Called by ContentView when `GhosttyRuntime` observes the system's
    /// displays waking from sleep. Reports it to the engine so a
    /// worker-pane spawn stranded by the sleep is redispatched
    /// immediately rather than waiting for the next periodic sweep.
    func spawnCapabilityRestored() {
        guard isAppSessionRegistered else { return }
        engine.sendSpawnCapabilityRestored()
    }

    /// Called by ContentView when a worker pane's libghostty surface fails to
    /// create so no shell ever comes up (the post-sleep "no active display"
    /// condition). NACKs the engine so it reaps the execution immediately and
    /// feeds its spawn-capability circuit breaker, instead of waiting out the
    /// 60s spawn-ack timeout.
    func workerPaneSpawnFailed(runId: String, reason: String) {
        guard isAppSessionRegistered else { return }
        engine.sendReportWorkerSpawnFailed(runId: runId, reason: reason)
    }
}
