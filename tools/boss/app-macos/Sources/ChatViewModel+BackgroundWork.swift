import Foundation

/// In-flight `ListEngineAttempts` call that opted into the background-work
/// snapshot. `replacesAttempts` is true for Activity's history refresh
/// (`limit > 0`) and false for the connection-scoped `limit = 0` poll, which
/// must not empty the Activity list.
struct BackgroundWorkPendingRequest {
    let generation: UInt64
    let replacesAttempts: Bool
}

extension ChatViewModel {
    /// Badge count is the engine snapshot's length verbatim. The app must
    /// not filter or re-count by kind.
    var backgroundWorkVisibleCount: Int { backgroundWork.count }

    var isBackgroundWorkPolling: Bool { backgroundWorkPollTask != nil }

    /// Start the connection-scoped five-second poll. Sends immediately,
    /// then every `backgroundWorkPollInterval` while connected. Restarts
    /// an existing timer so reconnect cannot leak a second loop.
    func startBackgroundWorkPolling() {
        stopBackgroundWorkPolling(clearSnapshot: false)
        sendBackgroundWorkPoll()
        let interval = backgroundWorkPollInterval
        backgroundWorkPollTask = Task { @MainActor [weak self] in
            while !Task.isCancelled {
                do {
                    try await Task.sleep(for: .seconds(interval))
                } catch {
                    return
                }
                guard let self, !Task.isCancelled, self.isConnected else { return }
                self.sendBackgroundWorkPoll()
            }
        }
    }

    /// Cancel the poller. When `clearSnapshot` is true, drop the published
    /// items so disconnect cannot leave stale chrome.
    func stopBackgroundWorkPolling(clearSnapshot: Bool) {
        backgroundWorkPollTask?.cancel()
        backgroundWorkPollTask = nil
        backgroundWorkPending.removeAll()
        backgroundWorkSendGeneration += 1
        backgroundWorkAppliedGeneration = backgroundWorkSendGeneration
        if clearSnapshot {
            backgroundWork = []
        }
    }

    /// Event-triggered extra poll. Does not reset the five-second timer;
    /// correctness still does not depend on the event arriving.
    func requestBackgroundWorkRefresh() {
        guard isConnected else { return }
        sendBackgroundWorkPoll()
    }

    func sendBackgroundWorkPoll() {
        let requestId = engine.sendListEngineAttempts(limit: 0, includeBackgroundWork: true)
        registerBackgroundWorkRequest(requestId: requestId, replacesAttempts: false)
    }

    func registerBackgroundWorkRequest(requestId: String, replacesAttempts: Bool) {
        backgroundWorkSendGeneration += 1
        backgroundWorkPending[requestId] = BackgroundWorkPendingRequest(
            generation: backgroundWorkSendGeneration,
            replacesAttempts: replacesAttempts
        )
    }

    /// Atomically replace the published snapshot. Older generations are
    /// dropped so a late poll cannot overwrite a newer refresh.
    @discardableResult
    func applyBackgroundWorkSnapshot(_ items: [BackgroundWorkItem], generation: UInt64) -> Bool {
        guard generation >= backgroundWorkAppliedGeneration else { return false }
        backgroundWorkAppliedGeneration = generation
        backgroundWork = items
        return true
    }

    func applyEngineAttemptsList(
        attempts: [EngineAttemptListEntry],
        backgroundWork: [BackgroundWorkItem],
        requestId: String?
    ) {
        let generation: UInt64
        let replacesAttempts: Bool
        if let requestId {
            guard isConnected else { return }
            guard let pending = backgroundWorkPending.removeValue(forKey: requestId) else { return }
            guard pending.generation >= backgroundWorkAppliedGeneration else { return }
            generation = pending.generation
            replacesAttempts = pending.replacesAttempts
        } else {
            backgroundWorkSendGeneration += 1
            generation = backgroundWorkSendGeneration
            replacesAttempts = true
        }
        guard applyBackgroundWorkSnapshot(backgroundWork, generation: generation) else { return }
        if replacesAttempts {
            engineAttempts = attempts
        }
    }
}
