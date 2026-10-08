import Foundation

extension ChatViewModel {
    // MARK: Operator questions (Doing ▸ Needs Attention)

    /// Answer the question a blocked worker asked on `task`'s card — the
    /// card's Yes / No buttons. Yes re-authorizes and restarts the task, No
    /// parks it in Backlog; either way the engine replies with
    /// `work_item_updated`, which moves the card, so nothing is applied
    /// optimistically here.
    ///
    /// Guards against a second click while the first is in flight, and
    /// surfaces a disconnected engine on the card instead of silently doing
    /// nothing. Returns whether a request was sent.
    @discardableResult
    func answerOperatorQuestion(for task: WorkTask, answer: OperatorAnswer) -> Bool {
        guard let question = task.operatorQuestion else { return false }
        guard operatorAnswerInFlightByTaskID[task.id] == nil else { return false }
        operatorAnswerErrorByTaskID.removeValue(forKey: task.id)
        operatorAnswerErrorQuestionIDByTaskID.removeValue(forKey: task.id)
        guard isConnected else {
            setOperatorAnswerError("Not connected to the engine — reconnect and try again.", taskID: task.id, questionID: question.id)
            return false
        }
        guard let requestID = engine.sendAnswerOperatorQuestion(id: question.id, answer: answer) else {
            setOperatorAnswerError("Couldn't send the answer to the engine. Try again.", taskID: task.id, questionID: question.id)
            return false
        }
        operatorAnswerInFlightByTaskID[task.id] = requestID
        return true
    }

    /// A typed refusal of an answer (`not_found`, `conflict`,
    /// `validation_failed`): re-enable the buttons and say why the click did
    /// nothing. The card is left in place — if the question was answered or
    /// withdrawn elsewhere, the `work_item_updated` that moved the task is
    /// what takes it out of Needs Attention.
    func handleOperatorQuestionError(message: String, requestId: String?) {
        guard let taskID = taskIDForOperatorAnswer(requestId: requestId) else {
            workErrorMessage = message
            return
        }
        operatorAnswerInFlightByTaskID.removeValue(forKey: taskID)
        setOperatorAnswerError(message, taskID: taskID, questionID: task(withID: taskID)?.operatorQuestion?.id)
    }

    /// Record a refusal against the question it was about, so a later
    /// question on the same task never shows it.
    private func setOperatorAnswerError(_ message: String, taskID: String, questionID: String?) {
        operatorAnswerErrorByTaskID[taskID] = message
        operatorAnswerErrorQuestionIDByTaskID[taskID] = questionID
    }

    /// The refusal text to paint under `task`'s buttons — only when it was
    /// recorded for the question the card currently shows.
    func operatorAnswerError(for task: WorkTask) -> String? {
        guard let message = operatorAnswerErrorByTaskID[task.id] else { return nil }
        if let recorded = operatorAnswerErrorQuestionIDByTaskID[task.id],
           recorded != task.operatorQuestion?.id {
            return nil
        }
        return message
    }

    /// A `work_error` whose request id is an in-flight answer belongs to that
    /// answer alone: settle it onto the card. Returns whether it matched.
    func settleOperatorAnswerWorkError(message: String, requestId: String?) -> Bool {
        guard let taskID = taskIDForOperatorAnswer(requestId: requestId) else { return false }
        abandonBackgroundWorkRequest(requestId: requestId)
        operatorAnswerInFlightByTaskID.removeValue(forKey: taskID)
        setOperatorAnswerError(message, taskID: taskID, questionID: task(withID: taskID)?.operatorQuestion?.id)
        workErrorMessage = message
        return true
    }

    private func taskIDForOperatorAnswer(requestId: String?) -> String? {
        guard let requestId else { return nil }
        return operatorAnswerInFlightByTaskID.first { $0.value == requestId }?.key
    }
}
