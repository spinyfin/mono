import Foundation

/// Session-registration handshake with the engine, plus the
/// spawn-capability-restored report ContentView pushes down after a
/// sleep/wake cycle. The engine reads the detached coordinator pane pid
/// itself; the visible tmux client is only a viewer and cannot be its
/// trust root.
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
        coordinatorUpdateAvailable = request.newerInstalledClaudeVersion
        refreshCoordinatorModelRecreateConfirmation()
    }

    func confirmCoordinatorModelRecreate() {
        guard let confirmation = coordinatorModelRecreateConfirmation else { return }
        coordinatorModelRecreateConfirmation = nil
        engine.sendRecreateCoordinator(expectedSpawnToken: confirmation.expectedSpawnToken, reason: .modelMismatch)
    }

    func cancelCoordinatorModelRecreate() {
        declinedCoordinatorRecreateToken = coordinatorModelRecreateConfirmation?.expectedSpawnToken
        coordinatorModelRecreateConfirmation = nil
    }

    /// Perform an operator-confirmed coordinator reset: destroys the current
    /// tmux session and starts a fresh one through the normal creation path
    /// (current binary, current instructions, no carried-over context). The
    /// UI-side confirmation dialog (Settings ▸ Workers, or the update-available
    /// banner) must call this only after the operator has explicitly confirmed
    /// — this method itself does not prompt. A no-op when no coordinator is
    /// currently attached (nothing to reset).
    func resetCoordinator() {
        guard let token = attachedCoordinatorSpawnToken else { return }
        engine.sendRecreateCoordinator(expectedSpawnToken: token, reason: .operatorReset)
    }

    private func refreshCoordinatorModelRecreateConfirmation() {
        guard let requested = requestedCoordinatorModel,
              let attached = attachedCoordinatorModel,
              let spawnToken = attachedCoordinatorSpawnToken,
              !attached.isEmpty,
              attached != requested,
              declinedCoordinatorRecreateToken != spawnToken
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

    /// Called by ContentView when `GhosttyRuntime` observes the system's
    /// displays waking from sleep. Reports it to the engine so a
    /// worker-pane spawn stranded by the sleep is redispatched
    /// immediately rather than waiting for the next periodic sweep.
    func spawnCapabilityRestored() {
        guard isAppSessionRegistered else { return }
        engine.sendSpawnCapabilityRestored()
    }
}
