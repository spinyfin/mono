import Foundation
import OSLog

private let operatorAnswerLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "operator-answer")

/// One answer (or dismissal) from the click until the board stops showing its
/// question. `ChatViewModel.operatorAnswerTimingByTaskID` holds the one in
/// progress for each task.
struct OperatorAnswerTiming: Equatable {
    let questionID: String
    let requestID: String
    let clickedAt: ContinuousClock.Instant
    /// When the engine's `work_item_updated` reply was applied to the model.
    var replyAppliedAt: ContinuousClock.Instant?
}

/// The app's three hops of an answer, one log line each, in the
/// `operator-answer` category: the click that sends the request, the engine's
/// reply applied to the model, and the question block leaving the card. The
/// engine logs its own hops for the same request id ("operator answer ..."),
/// so one answer can be followed from the click to the repaint:
///
///     log stream --predicate 'subsystem == "dev.spinyfin.bossmacapp" AND category == "operator-answer"'
extension ChatViewModel {
    /// How long a reply may wait for its card to repaint before the timing is
    /// dropped. A card that is off screen never reports that it cleared.
    static let operatorAnswerClearedDeadline: Duration = .seconds(10)

    func beginOperatorAnswerTiming(taskID: String, questionID: String, requestID: String, isDismissal: Bool) {
        operatorAnswerTimingByTaskID[taskID] = OperatorAnswerTiming(
            questionID: questionID, requestID: requestID, clickedAt: .now)
        let kind = isDismissal ? "dismiss" : "answer"
        operatorAnswerLog.info(
            "\(kind, privacy: .public) click sent: task=\(taskID, privacy: .public) question=\(questionID, privacy: .public) request=\(requestID, privacy: .public)")
    }

    /// A task update was applied to the model. It is the reply to the answer
    /// being timed once the model no longer holds the answered question; an
    /// unrelated update to the same task leaves the question in place.
    func recordOperatorAnswerReplyApplied(taskID: String) {
        guard var timing = operatorAnswerTimingByTaskID[taskID], timing.replyAppliedAt == nil,
              task(withID: taskID)?.operatorQuestion?.id != timing.questionID
        else { return }
        let now = ContinuousClock.now
        timing.replyAppliedAt = now
        operatorAnswerTimingByTaskID[taskID] = timing
        let elapsed = timing.clickedAt.duration(to: now)
        operatorAnswerLog.info(
            "answer reply applied: task=\(taskID, privacy: .public) request=\(timing.requestID, privacy: .public) since_click=\(String(describing: elapsed), privacy: .public)")
        let requestID = timing.requestID
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: ChatViewModel.operatorAnswerClearedDeadline)
            self?.expireOperatorAnswerTiming(taskID: taskID, requestID: requestID)
        }
    }

    /// The engine refused the answer, or the request failed: the question
    /// stays on the card, so there is no repaint to wait for.
    func recordOperatorAnswerRefused(taskID: String) {
        guard let timing = operatorAnswerTimingByTaskID.removeValue(forKey: taskID) else { return }
        let elapsed = timing.clickedAt.duration(to: .now)
        operatorAnswerLog.info(
            "answer refused: task=\(taskID, privacy: .public) request=\(timing.requestID, privacy: .public) since_click=\(String(describing: elapsed), privacy: .public)")
    }

    /// The question block on `taskID`'s card left the view tree. It also
    /// leaves when its card scrolls away or its column is rebuilt, so this
    /// counts only once the model no longer holds the answered question.
    func recordOperatorQuestionCardCleared(taskID: String) {
        guard let timing = operatorAnswerTimingByTaskID[taskID],
              task(withID: taskID)?.operatorQuestion?.id != timing.questionID
        else { return }
        operatorAnswerTimingByTaskID.removeValue(forKey: taskID)
        let now = ContinuousClock.now
        let sinceClick = timing.clickedAt.duration(to: now)
        let sinceReply = timing.replyAppliedAt.map { String(describing: $0.duration(to: now)) } ?? "no reply yet"
        operatorAnswerLog.info(
            "question card cleared: task=\(taskID, privacy: .public) request=\(timing.requestID, privacy: .public) since_click=\(String(describing: sinceClick), privacy: .public) since_reply=\(sinceReply, privacy: .public)")
    }

    func expireOperatorAnswerTiming(taskID: String, requestID: String) {
        guard operatorAnswerTimingByTaskID[taskID]?.requestID == requestID else { return }
        operatorAnswerTimingByTaskID.removeValue(forKey: taskID)
        operatorAnswerLog.info(
            "question card not cleared \(String(describing: ChatViewModel.operatorAnswerClearedDeadline), privacy: .public) after the reply (card off screen, or the board did not repaint): task=\(taskID, privacy: .public) request=\(requestID, privacy: .public)")
    }
}
