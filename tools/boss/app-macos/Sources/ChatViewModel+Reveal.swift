import Foundation

extension ChatViewModel {
    @discardableResult
    func prepareReveal(_ taskID: String) -> RevealCardResult {
        if navigator.mode == .navigator {
            if let task = task(withID: taskID), let projection = task.navigator,
               projection.version == 1, let root = self.task(withID: projection.rootTaskId),
               root.navigator?.root?.knownSection != nil, navigatorIncludes(root) {
                selectedWorkCardID = root.id
                selectNavigatorRoot(taskID)
                navigatorRevealRootID = root.id
                revealNavigatorSelection()
                armRevealDeadline(taskID: taskID, waitingForTree: false)
                return .revealed(cardID: root.id)
            }
            navigator.mode = .kanban
            defaults.set(WorkViewMode.kanban.rawValue, forKey: "boss.work.viewMode")
        }
        // Reveal's contract is "show me this card", so a parked target turns
        // "Show parked" on for the session (not persisted: it is not the
        // user's standing preference).
        if !showParked, let target = task(withID: taskID), isParked(target) {
            showParked = true
        }
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
        revealDeadlineScheduler { [weak self] in
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
        navigatorRevealRootID = nil
        let completion = revealCompletion
        revealCompletion = nil
        revealScrollTarget = nil
        pendingRevealScrollID = nil
        pendingRevealReconcilesProjectFilter = false
        revealProductID = nil
        completion?(result)
    }
}
