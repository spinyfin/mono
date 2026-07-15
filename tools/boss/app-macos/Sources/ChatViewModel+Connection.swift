import Foundation

/// Connection-lifecycle handling split out of `ChatViewModel.handle(_:)`:
/// the `.resyncRequired` refetch path and the debounced "connection lost"
/// banner scheduling. See `showConnectionLostBanner` in `ChatViewModel.swift`
/// for why the banner is debounced rather than tied directly to `isConnected`.
extension ChatViewModel {
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
