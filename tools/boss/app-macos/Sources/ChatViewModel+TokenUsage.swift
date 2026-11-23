import Foundation

extension ChatViewModel {
    /// One round trip for the Token Usage window. A generation counter
    /// drops a reply that belongs to an earlier toolbar change.
    func requestTokenUsage(_ query: TokenUsageQuery) {
        tokenUsage.generation += 1
        let generation = tokenUsage.generation
        tokenUsage.loading = true
        tokenUsage.error = nil
        let requestId = engine.sendGetTokenUsageRows(query)
        tokenUsage.pendingRequestId = requestId
        Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 45_000_000_000)
            guard let self, self.tokenUsage.generation == generation,
                  self.tokenUsage.pendingRequestId != nil
            else { return }
            self.tokenUsage.loading = false
            self.tokenUsage.error = "The engine did not answer in time."
        }
    }

    func receiveTokenUsageRows(_ report: TokenUsageRowsReport, requestId: String?) {
        if let requestId, let pending = tokenUsage.pendingRequestId, requestId != pending {
            return
        }
        tokenUsage.report = report
        tokenUsage.loading = false
        tokenUsage.error = nil
        tokenUsage.receivedAt = Date()
        tokenUsage.pendingRequestId = nil
    }

    /// Keep the last reply on screen (dimmed by the window) and show the
    /// engine's reason inline. Returns true when this error belonged to
    /// the in-flight usage request.
    func receiveTokenUsageError(message: String, requestId: String?) -> Bool {
        if let requestId {
            guard requestId == tokenUsage.pendingRequestId else { return false }
        } else if tokenUsage.pendingRequestId == nil, !tokenUsage.loading {
            return false
        }
        tokenUsage.loading = false
        tokenUsage.error = message
        tokenUsage.pendingRequestId = nil
        return true
    }

    func failTokenUsageOnDisconnect() {
        guard tokenUsage.pendingRequestId != nil || tokenUsage.loading else { return }
        tokenUsage.loading = false
        tokenUsage.error = "Connection lost while loading token usage."
        tokenUsage.pendingRequestId = nil
    }
}
