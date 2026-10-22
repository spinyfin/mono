import Foundation

/// Connection-lifecycle handling split out of `ChatViewModel.handle(_:)`:
/// initial/reconnect reads and "connection lost"
/// banner scheduling. See `showConnectionLostBanner` in `ChatViewModel.swift`
/// for why the banner is debounced rather than tied directly to `isConnected`.
extension ChatViewModel {
    /// Restore the session and refresh only data not supplied by subscription.
    func handleConnected() {
        let flow: PopulationFlow = hasConnectedOnce ? .reconnect : .coldStart
        UpdateLifecycle.recordStartup("engine-connected")
        // This burst re-reads everything a pending resync refetch would.
        cancelPendingResyncRefetch()
        markConnected()
        resetConnectionLostBanner()
        if refreshUnsentActions() { showDisconnectedActionNotice() }
        // A quota request in flight when the socket dropped will never be
        // answered, so clear the pending flag rather than leaving the
        // pane's Refresh button disabled for the rest of the session.
        isRefreshingDriverQuota = false
        engine.sendRegisterAppSession()
        refreshWorkSubscriptions()
        // Re-subscribe any open markdown viewers' comment topics and reload
        // them; the engine dropped every subscription on the disconnect.
        commentBridge.handleReconnected()
        // Subscribe currently acknowledges topics only (engine app/subscriptions.rs).
        // Publishers emit changes, not snapshots on subscribe, so all of these
        // reads are required on reconnect too, including after an engine restart.
        engine.sendListProducts()
        engine.sendListWorkerLiveStates()
        engine.sendListLiveStatusDisabledSlots()
        // Pull the engine's configuration health on every (re)connect
        // so the top-of-window banner reflects the *current* engine,
        // not the one we attached to before a restart (#699).
        engine.sendGetEngineHealth()
        // Pull the current per-installation settings snapshot so
        // Settings reflects the engine immediately after connecting
        // rather than waiting until Settings is opened.
        engine.sendGetSettings()
        refreshOffsiteBackup()
        // Pull the current GitHub OAuth auth state so the "GitHub
        // account" settings subsection reflects a token persisted by a
        // prior session (the engine restores it from the keychain at
        // boot) without waiting for a device-flow transition.
        engine.sendGitHubAuthStatus()
        if let productID = currentSelectedProductID, engine.claimConnectTree(productId: productID) {
            engine.sendGetWorkTree(productId: productID, flow: flow)
        }
        startBackgroundWorkPolling()
    }

    /// Leave the initiating sheet or gesture intact when the retry queue is full.
    func canSubmitUserActions(_ count: Int = 1, describing action: String = "This action") -> Bool {
        guard engine.hasRoomForDisconnectedActions(count) else {
            showActionNotSaved(requestKind: action)
            return false
        }
        unsavedActionNotice = nil
        showDisconnectedActionNotice()
        return true
    }

    func showActionNotSaved(requestKind: String) {
        unsavedActionNotice = "Too many pending actions; \(requestKind) was not saved. Keep your draft and retry after sending or discarding pending actions."
        showDisconnectedActionNotice()
    }
    /// Retained actions require an explicit retry after automatic reconnect.
    func showDisconnectedActionNotice() {
        var notices = [String]()
        if let uncertainActionNotice { notices.append(uncertainActionNotice) }
        if let unsavedActionNotice { notices.append(unsavedActionNotice) }
        if retryingDisconnectedActions { notices.append("Sending pending actions…") }
        if engine.hasUnsentActions {
            notices.append(isConnected
                ? "Connection restored. Retry to send your pending actions."
                : "Lost connection to the engine — reconnecting… Your actions are saved; try again with Retry once connected.")
        }
        disconnectedActionNotice = notices.isEmpty ? nil : notices.joined(separator: "\n")
    }

    /// Re-reads the engine's retained-action state into the published
    /// mirror. Returns the fresh value.
    @discardableResult
    func refreshUnsentActions() -> Bool {
        let value = engine.hasUnsentActions
        if hasUnsentActions != value { hasUnsentActions = value }
        return value
    }

    func retryDisconnectedActions() {
        guard refreshUnsentActions() else { return }
        let retryingCoordinatorReset = engine.hasUnsentAction(kind: "recreate_coordinator") && pendingCoordinatorReset != nil
        if engine.retryUnsentActions() {
            if retryingCoordinatorReset { coordinatorResetWaiting = true }
            retryingDisconnectedActions = true
            unsavedActionNotice = nil
            showDisconnectedActionNotice()
        } else {
            showDisconnectedActionNotice()
        }
        refreshUnsentActions()
    }

    /// The user has checked the reported uncertain outcomes. Queued and
    /// retried actions are untouched; only the uncertainty is cleared.
    func acknowledgeUncertainActions() {
        engine.acknowledgeUncertainActions()
        uncertainActionMessages.removeAll()
        uncertainActionNotice = nil
        showDisconnectedActionNotice()
    }

    func dismissDisconnectedActions() {
        let discardedDragIDs = engine.unsentDragTaskIDs()
        let discardingCoordinatorReset = engine.hasUnsentAction(kind: "recreate_coordinator")
        engine.discardUnsentActions()
        retryingDisconnectedActions = engine.hasRetriedActions
        if !discardedDragIDs.isEmpty {
            for id in discardedDragIDs { pendingDragAdmissionChecks[id] = nil }
            bounceBackOptimisticMoves(message: nil, only: discardedDragIDs)
        }
        unsavedActionNotice = nil
        if discardingCoordinatorReset { clearCoordinatorReset() }
        refreshUnsentActions()
        showDisconnectedActionNotice()
    }

    /// Clears the banner and supersedes any pending delayed reveal on a
    /// successful (re)connect.
    func resetConnectionLostBanner() {
        showConnectionLostBanner = false
        connectionGeneration += 1
    }

    /// Arms the delayed reveal of `showConnectionLostBanner` after a
    /// `.disconnected` event. `connectionGeneration` is bumped on every
    /// connect/disconnect transition; capturing it here lets the closure
    /// recognize a superseding reconnect and no-op instead of popping the
    /// banner after the app already recovered.
    func scheduleConnectionLostBannerCheck() {
        connectionGeneration += 1
        let generation = connectionGeneration
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.connectionLostBannerDelay) { [weak self] in
            guard let self, self.connectionGeneration == generation, !self.isConnected, self.hasConnectedOnce else { return }
            self.showConnectionLostBanner = true
        }
    }
}
