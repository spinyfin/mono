import Foundation

/// Connection-lifecycle handling split out of `ChatViewModel.handle(_:)`:
/// the `.resyncRequired` refetch path and the debounced "connection lost"
/// banner scheduling. See `showConnectionLostBanner` in `ChatViewModel.swift`
/// for why the banner is debounced rather than tied directly to `isConnected`.
extension ChatViewModel {
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
            bounceBackOptimisticMoves(only: discardedDragIDs, message: nil)
        }
        unsavedActionNotice = nil
        if discardingCoordinatorReset { clearCoordinatorReset() }
        refreshUnsentActions()
        showDisconnectedActionNotice()
    }

    /// The engine dropped pending invalidations for our session while
    /// riding out a publish burst instead of disconnecting us (see
    /// `EngineEvent.resyncRequired`). The socket never went down, so
    /// there's no connection state to fix — just refetch whatever a
    /// dropped invalidation could have covered, the same data a
    /// `.workInvalidated` refresh would have pulled.
    func handleResyncRequired() {
        engine.sendListProducts()
        engine.sendListWorkerLiveStates()
        engine.sendListLiveStatusDisabledSlots()
        commentBridge.reloadOpenLayers()
        if let productID = currentSelectedProductID {
            engine.sendGetWorkTree(productId: productID, flow: .invalidationRefetch)
            engine.sendListAttentionItemsForWorkItem(workItemID: productID)
            engine.sendListAttentionGroups(productId: productID)
            engine.sendListDeferredScopeAttentions(productId: productID)
            refreshPlannerRuns(forProductID: productID)
        }
        requestBackgroundWorkRefresh()
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
