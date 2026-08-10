import Foundation

/// Session-registration handshake with the engine and worker-pane lifecycle
/// reports ContentView pushes down as libghostty surfaces come and go. The
/// engine reads the detached coordinator pane pid itself; the visible tmux
/// client is only a viewer and cannot be its trust root.
extension ChatViewModel {
    func coordinatorModelConfigured(_ model: String) {
        let model = model.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !model.isEmpty else { return }
        requestedCoordinatorModel = model
        refreshCoordinatorModelRecreateConfirmation()
    }

    func coordinatorPaneAttached(_ request: EngineCoordinatorAttachRequest) {
        attachedCoordinatorModel = request.model
        attachedCoordinatorSpawnToken = request.spawnToken
        refreshCoordinatorModelRecreateConfirmation()
    }

    func confirmCoordinatorModelRecreate() {
        guard let confirmation = coordinatorModelRecreateConfirmation else { return }
        coordinatorModelRecreateConfirmation = nil
        engine.sendRecreateCoordinator(expectedSpawnToken: confirmation.expectedSpawnToken)
    }

    func cancelCoordinatorModelRecreate() {
        coordinatorModelRecreateConfirmation = nil
    }

    private func refreshCoordinatorModelRecreateConfirmation() {
        guard let requested = requestedCoordinatorModel,
              let attached = attachedCoordinatorModel,
              let spawnToken = attachedCoordinatorSpawnToken,
              !attached.isEmpty,
              attached != requested
        else {
            coordinatorModelRecreateConfirmation = nil
            return
        }
        coordinatorModelRecreateConfirmation = CoordinatorModelRecreateConfirmation(
            currentModel: attached,
            requestedModel: requested,
            expectedSpawnToken: spawnToken
        )
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
    func workerPaneDied(runId: String, reason: WorkerPaneDeathReason) {
        guard isAppSessionRegistered else { return }
        engine.sendWorkerPaneDied(runId: runId, reason: reason)
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
