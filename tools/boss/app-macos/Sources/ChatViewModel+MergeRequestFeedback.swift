import Foundation
import OSLog

private let mergeRequestLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "merge-request")

extension ChatViewModel {
    func failOutstandingMergeRequests(message: String) {
        for taskID in mergingWhenReadyIDs {
            mergeErrorNoticesByTaskID[taskID] = message
        }
        mergingWhenReadyIDs.removeAll()
        mergeRequestStartedAt.removeAll()
        mergeRevisionConfirmationOrigins.removeAll()
    }

    func beginMergeRequestFeedback(for taskID: String) {
        let started = ContinuousClock.now
        mergeRequestStartedAt[taskID] = started
        mergeRequestLog.info("merge click: task=\(taskID, privacy: .public)")
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: .seconds(60))
            self?.expireMergeRequest(for: taskID, startedAt: started)
        }
    }

    func expireMergeRequest(for taskID: String, startedAt: ContinuousClock.Instant) {
        guard mergingWhenReadyIDs.contains(taskID), mergeRequestStartedAt[taskID] == startedAt else { return }
        mergingWhenReadyIDs.remove(taskID)
        mergeRequestStartedAt.removeValue(forKey: taskID)
        mergeRevisionConfirmationOrigins.removeValue(forKey: taskID)
        mergeErrorNoticesByTaskID[taskID] = "Merge request timed out. Its outcome is unknown; check the PR before retrying."
        mergeRequestLog.error("merge request timed out: task=\(taskID, privacy: .public)")
    }

    func recordMergeAcknowledgement(for taskID: String) {
        if let started = mergeRequestStartedAt.removeValue(forKey: taskID) {
            let elapsed = started.duration(to: .now)
            mergeRequestLog.info("merge acknowledgement applied to banner: task=\(taskID, privacy: .public) elapsed=\(String(describing: elapsed), privacy: .public)")
        }
    }
}
