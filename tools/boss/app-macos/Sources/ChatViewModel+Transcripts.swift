import Foundation

/// Execution history, attachments, transcripts, and live-status toggles.
extension ChatViewModel {
    /// Fetch the execution history for `taskId` from the engine.
    /// Clears any cached rows first so the viewer shows a loading state.
    /// The engine replies with an `executions_list` event that populates
    /// [[executionsByTaskID]].
    func loadExecutions(taskId: String) {
        executionsByTaskID[taskId] = nil
        executionsLoadFailureByTaskID.removeValue(forKey: taskId)
        executionsInFlightTaskIDs.insert(taskId)
        engine.sendListExecutions(taskId: taskId)
    }

    /// Fetch the screenshot evidence for `taskId`'s revision chain from the
    /// engine. Clears any cached rows first so the viewer shows a loading
    /// state. The engine replies with an `attachments_list` event that
    /// populates [[attachmentsByTaskID]].
    func loadAttachments(taskId: String) {
        attachmentsByTaskID[taskId] = nil
        attachmentsLoadFailureByTaskID.removeValue(forKey: taskId)
        attachmentsInFlightTaskIDs.insert(taskId)
        engine.sendListAttachmentsForWorkItem(taskId: taskId)
    }

    /// Fetch the rendered transcript for `executionId` the first time it is
    /// requested. Selecting an execution in the viewer calls this; an
    /// already-loaded, in-flight, or unavailable transcript is left
    /// untouched so re-selecting a row doesn't re-hit the engine. Use
    /// [[refreshTranscript(executionId:)]] to force a re-fetch.
    func loadTranscript(executionId: String) {
        if transcriptsByExecutionID[executionId] != nil { return }
        transcriptsByExecutionID[executionId] = .loading
        engine.sendExecutionTranscript(executionId: executionId)
    }

    /// Force a re-fetch of `executionId`'s transcript — the "Refresh"
    /// affordance on a still-running (live) execution, and the periodic
    /// poll while a live transcript's view is open. Deliberately leaves an
    /// already-`.loaded` doc in place while the fetch is in flight instead
    /// of flipping to `.loading`: swapping to the loading placeholder and
    /// back tears down and remounts `TranscriptView`, which resets its
    /// scroll position and per-segment expansion state on every refresh.
    /// Only [[loadTranscript(executionId:)]]'s first fetch (nothing loaded
    /// yet) needs the loading placeholder.
    func refreshTranscript(executionId: String) {
        if transcriptsByExecutionID[executionId] == nil {
            transcriptsByExecutionID[executionId] = .loading
        }
        engine.sendExecutionTranscript(executionId: executionId)
    }

    /// Toggle the live-status summarizer for `slotId`. Sends the
    /// RPC and optimistically updates local state; the engine echo
    /// brings the two back in sync.
    func setLiveStatusEnabled(slotId: Int, enabled: Bool) {
        if enabled {
            liveStatusDisabledSlotIDs.remove(slotId)
        } else {
            liveStatusDisabledSlotIDs.insert(slotId)
        }
        engine.sendSetLiveStatusEnabled(slotId: slotId, enabled: enabled)
    }

    /// `true` if the live-status summarizer is currently enabled for
    /// `slotId`. Defaults to enabled — the disabled set is the
    /// minority case.
    func isLiveStatusEnabled(slotId: Int) -> Bool {
        !liveStatusDisabledSlotIDs.contains(slotId)
    }
}
