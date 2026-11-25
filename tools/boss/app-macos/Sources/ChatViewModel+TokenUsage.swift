import Foundation

extension ChatViewModel {
    /// One round trip for the Token Usage window. A generation counter and
    /// expected request id drop any reply that belongs to an earlier query.
    /// Skipped while disconnected; the window re-requests on reconnect.
    func requestTokenUsage(_ query: TokenUsageQuery) {
        guard isConnected else {
            return
        }
        let requestId = engine.sendGetTokenUsageRows(query)
        guard let generation = tokenUsage.beginRequest(requestId: requestId) else {
            return
        }
        Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 45_000_000_000)
            guard let self else { return }
            _ = self.tokenUsage.failOnTimeout(generation: generation)
        }
    }

    /// Drop any in-flight reply while a debounced toolbar change is pending.
    func invalidatePendingTokenUsage() {
        tokenUsage.invalidatePending()
    }

    func receiveTokenUsageRows(_ report: TokenUsageRowsReport, requestId: String?) {
        _ = tokenUsage.receiveRows(report, requestId: requestId)
    }

    /// Keep the last reply on screen (dimmed by the window) and show the
    /// engine's reason inline. Returns true only when this error's request id
    /// matches the in-flight usage request.
    func receiveTokenUsageError(message: String, requestId: String?) -> Bool {
        tokenUsage.receiveError(message: message, requestId: requestId)
    }

    func failTokenUsageOnDisconnect() {
        tokenUsage.failOnDisconnect()
    }
}
