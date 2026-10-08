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
        guard isConnected else {
            operatorAnswerErrorByTaskID[task.id] = "Not connected to the engine — reconnect and try again."
            return false
        }
        guard let requestID = engine.sendAnswerOperatorQuestion(id: question.id, answer: answer) else {
            operatorAnswerErrorByTaskID[task.id] = "Couldn't send the answer to the engine. Try again."
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
        operatorAnswerErrorByTaskID[taskID] = message
    }

    /// A generic `work_error` that answers an in-flight answer request must
    /// not leave its buttons disabled forever. The error text itself is
    /// surfaced by the caller's normal `work_error` path.
    func clearOperatorAnswerInFlight(requestId: String?) {
        guard let taskID = taskIDForOperatorAnswer(requestId: requestId) else { return }
        operatorAnswerInFlightByTaskID.removeValue(forKey: taskID)
    }

    private func taskIDForOperatorAnswer(requestId: String?) -> String? {
        guard let requestId else { return nil }
        return operatorAnswerInFlightByTaskID.first { $0.value == requestId }?.key
    }
}
