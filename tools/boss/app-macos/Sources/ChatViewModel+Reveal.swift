import Foundation

extension ChatViewModel {
    @discardableResult
    func prepareReveal(_ taskID: String) -> RevealCardResult {
        let outcome = revealCardTarget(for: taskID)
        guard case .revealed(let cardID) = outcome else {
            let reason: String
            if case .unreachable(let detail) = outcome {
                reason = detail
            } else {
                reason = "no card for \(taskID) in the loaded product work tree"
            }
            finishReveal(.failure(.internalFailure(reason)))
            return .unreachable(reason: reason)
        }
        guard WorkBoardColumnKey.allCases.contains(where: { column in
            workSections(in: column).contains { $0.items.contains { $0.id == cardID } }
        }) else {
            let reason = "\(taskID) has no rendered card in the current board"
            finishReveal(.failure(.internalFailure(reason)))
            return .unreachable(reason: reason)
        }
        selectedWorkCardID = cardID
        revealScrollTarget = cardID
        return outcome
    }

    func confirmReveal(cardID: String, generation: UUID) {
        guard generation == revealGeneration, revealScrollTarget == cardID else { return }
        revealHighlightID = cardID
        finishReveal(.success)
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in
            guard let self, self.revealGeneration == generation else { return }
            self.revealHighlightID = nil
        }
    }

    func finishReveal(_ result: EngineRevealResult) {
        let completion = revealCompletion
        revealCompletion = nil
        revealScrollTarget = nil
        pendingRevealScrollID = nil
        revealProductID = nil
        completion?(result)
    }
}
