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
        armRevealDeadline(taskID: taskID, waitingForTree: false)
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

    func armRevealDeadline(taskID: String, waitingForTree: Bool) {
        let generation = revealGeneration
        let token = UUID()
        revealDeadlineToken = token
        DispatchQueue.main.asyncAfter(deadline: .now() + 3) { [weak self] in
            guard let self, self.revealGeneration == generation,
                  self.revealDeadlineToken == token else { return }
            self.finishReveal(.failure(.internalFailure(
                "could not reveal \(taskID): " + (waitingForTree
                    ? "target product work tree did not arrive"
                    : "target card did not become visible in the board viewport")
            )))
        }
    }

    func finishReveal(_ result: EngineRevealResult) {
        revealDeadlineToken = UUID()
        let completion = revealCompletion
        revealCompletion = nil
        revealScrollTarget = nil
        pendingRevealScrollID = nil
        revealProductID = nil
        completion?(result)
    }
}
