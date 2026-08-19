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
        attemptsAppliedGeneration = backgroundWorkSendGeneration
        if clearSnapshot {
            backgroundWork = []
        }
    }

    /// Event-triggered extra poll. Does not reset the five-second timer;
    /// correctness still does not depend on the event arriving. Skipped
    /// while a background-only request is already in flight so an
    /// invalidation burst cannot stack snapshot RPCs on top of the cadence.
    func requestBackgroundWorkRefresh() {
        guard isConnected else { return }
        if backgroundWorkPending.values.contains(where: { !$0.replacesAttempts }) {
            return
        }
        sendBackgroundWorkPoll()
    }

    func sendBackgroundWorkPoll() {
        guard let requestId = engine.sendListEngineAttempts(limit: 0, includeBackgroundWork: true) else {
            return
        }
        registerBackgroundWorkRequest(requestId: requestId, replacesAttempts: false)
    }

    func registerBackgroundWorkRequest(requestId: String, replacesAttempts: Bool) {
        backgroundWorkSendGeneration += 1
        backgroundWorkPending[requestId] = BackgroundWorkPendingRequest(
            generation: backgroundWorkSendGeneration,
            replacesAttempts: replacesAttempts
        )
    }

    /// Drop in-flight entries that can no longer win either payload
    /// gate. A matching reply that never arrives (work_error, or a
    /// `sendLine` that did not write) would otherwise stay until
    /// disconnect.
    func pruneStaleBackgroundWorkPending() {
        backgroundWorkPending = backgroundWorkPending.filter { _, pending in
            let canWinSnapshot = pending.generation >= backgroundWorkAppliedGeneration
            let canWinAttempts = pending.replacesAttempts
                && pending.generation >= attemptsAppliedGeneration
            return canWinSnapshot || canWinAttempts
        }
    }

    /// Atomically replace the published snapshot. Older generations are
    /// dropped so a late poll cannot overwrite a newer refresh.
    @discardableResult
    func applyBackgroundWorkSnapshot(_ items: [BackgroundWorkItem], generation: UInt64) -> Bool {
        guard generation >= backgroundWorkAppliedGeneration else { return false }
        backgroundWorkAppliedGeneration = generation
        backgroundWork = items
        pruneStaleBackgroundWorkPending()
        return true
    }

    /// Apply a `ListEngineAttempts` reply. Snapshot and Activity list
    /// are gated independently so a late history refresh that loses the
    /// snapshot race still updates `engineAttempts`. Replies without a
    /// request id are ignored — the engine always echoes one.
    func applyEngineAttemptsList(
        attempts: [EngineAttemptListEntry],
        backgroundWork: [BackgroundWorkItem],
        requestId: String?
    ) {
        guard isConnected else { return }
        guard let requestId else { return }
        guard let pending = backgroundWorkPending.removeValue(forKey: requestId) else { return }
        if pending.generation >= backgroundWorkAppliedGeneration {
            applyBackgroundWorkSnapshot(backgroundWork, generation: pending.generation)
        }
        if pending.replacesAttempts, pending.generation >= attemptsAppliedGeneration {
            attemptsAppliedGeneration = pending.generation
            engineAttempts = attempts
        }
        pruneStaleBackgroundWorkPending()
    }

    /// Forget a pending snapshot request that will never yield
    /// `engine_attempts_list` (the engine answered with `work_error`).
    func abandonBackgroundWorkRequest(requestId: String?) {
        guard let requestId else { return }
        backgroundWorkPending.removeValue(forKey: requestId)
    }
}
