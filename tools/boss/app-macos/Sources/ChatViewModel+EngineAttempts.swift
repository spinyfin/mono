import Foundation

/// Bookkeeping for Activity's `list_engine_attempts` history reads: which are
/// in flight, and which reply is allowed to replace `engineAttempts`.
extension ChatViewModel {
    func registerEngineAttemptsRequest(requestId: String) {
        if remediationEventsDuringRead[requestId] == nil {
            remediationEventsDuringRead[requestId] = []
        }
        engineAttemptsSendGeneration += 1
        engineAttemptsPending[requestId] = engineAttemptsSendGeneration
    }

    /// Forget the reads the dropped connection carried: no reply is coming
    /// for them. Reads still queued for the retry after reconnect are kept,
    /// re-registered at a generation newer than the disconnect barrier, so
    /// repeated failed reconnects neither orphan them nor make them look
    /// older than it.
    func dropSentEngineAttemptsReads() {
        let queued = engineAttemptsPending.filter { engine.isReadQueued(requestId: $0.key) }
        engineAttemptsPending.removeAll()
        engineAttemptsSendGeneration += 1
        engineAttemptsAppliedGeneration = engineAttemptsSendGeneration
        for requestId in queued.sorted(by: { $0.value < $1.value }).map(\.key) {
            registerEngineAttemptsRequest(requestId: requestId)
        }
    }

    /// Drop in-flight entries that can no longer win the generation gate. A
    /// matching reply that never arrives (work_error, or a `sendLine` that
    /// did not write) would otherwise stay until disconnect.
    func pruneStaleEngineAttemptsPending() {
        engineAttemptsPending = engineAttemptsPending.filter { _, generation in
            generation >= engineAttemptsAppliedGeneration
        }
        remediationEventsDuringRead = remediationEventsDuringRead.filter { id, _ in
            id == remediationStateRequestID || engineAttemptsPending[id] != nil
        }
    }

    /// Apply a `ListEngineAttempts` reply. Replies without a request id are
    /// ignored — the engine always echoes one.
    func applyEngineAttemptsList(attempts: [EngineAttemptListEntry], requestId: String?) {
        guard isConnected else { return }
        guard let requestId else { return }
        if requestId == remediationStateRequestID {
            applyOpenRemediationState(attempts, requestID: requestId)
            return
        }
        // Keep this request registered while applying: pruning must retain
        // its buffered pushes until the history has been replaced.
        guard let generation = engineAttemptsPending[requestId] else { return }
        if generation >= engineAttemptsAppliedGeneration {
            engineAttemptsAppliedGeneration = generation
            engineAttempts = attempts
        }
        engineAttemptsPending.removeValue(forKey: requestId)
        replayRemediationEventsIntoHistory(for: requestId)
        pruneStaleEngineAttemptsPending()
    }

    /// Forget a pending read that will never yield `engine_attempts_list`
    /// (the engine answered with `work_error`).
    func abandonEngineAttemptsRequest(requestId: String?) {
        guard let requestId else { return }
        engineAttemptsPending.removeValue(forKey: requestId)
        remediationEventsDuringRead.removeValue(forKey: requestId)
        if remediationStateRequestID == requestId { remediationStateRequestID = nil }
    }
}
