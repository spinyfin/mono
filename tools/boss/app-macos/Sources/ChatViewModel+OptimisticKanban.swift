import Foundation

extension ChatViewModel {
    struct DragRefusalNotice: Equatable {
        let taskID: String
        let message: String
    }

    /// Handle a kanban drop. Reports **where** the card landed and lets the
    /// engine decide what that drop meant; returns whether the drop was
    /// forwarded at all, so the source lane can render an inline warning for
    /// the one refusal the client can answer without a round trip.
    ///
    /// This does not compute a status, and deliberately so. Both board levels
    /// — column, and group within the Done column — are derived from engine
    /// state, so the same drop target means different things depending on
    /// where the card already was, and the client sees only the layout. See
    /// the `boss-engine-board-gesture` crate docs for the full argument and
    /// the resolution matrix.
    ///
    /// `group` is `nil` when the drop landed on the column but not on one of
    /// its groups. That is strictly less intent than a group-qualified drop,
    /// and the engine treats it as such.
    func attemptDrop(_ taskID: String, onColumn column: WorkBoardColumnKey, group: WorkBoardGroupKey?) -> Bool {
        guard let task = task(withID: taskID) else { return false }

        // Whether the card visibly moved. This is a *rendering* fact the
        // client already owns (it is how the board lays sections out), not a
        // status decision — it gates the pre-flight warning and the
        // optimistic reposition below, never what gets sent. A drop that
        // leaves the card where it is has nothing to warn about and nothing
        // to animate; the engine still resolves it (and logs the reorder).
        let origin = effectiveBoardColumn(for: task)
        let originGroup = boardGroup(for: task)
        let staysPut = origin == column && (group == nil || originGroup == group)

        if !staysPut,
           task.status == "blocked",
           hasGatingPrereqs(task)
        {
            let count = gatingPrereqs(for: task.id).count
            let plural = count == 1 ? "prerequisite" : "prerequisites"
            dragRefusalNotice = DragRefusalNotice(
                taskID: task.id,
                message: "\(task.name) is gated by \(count) incomplete \(plural) — clear them or remove the edge first."
            )
            scheduleDragRefusalDismiss(for: task.id)
            return false
        }

        // Moving out of Doing while a live worker is attached is blocked
        // except for two intentional gestures — see `moveTask`, which applies
        // the same rule to the popover's explicit Move buttons.
        if !staysPut,
           origin == .doing,
           column != .backlog,
           column != .done,
           hasLiveWorker(forTaskID: taskID)
        {
            appendSystemMessage(
                "\(task.name) is being worked on by a live worker. Stop the worker before moving the card out of Doing.",
                alwaysShow: true
            )
            return false
        }

        if !staysPut {
            // Optimistic reposition: draw the card at the drop site until the
            // engine's answer arrives. `bounceBackOptimisticMoves` returns it
            // to `origin` if the engine refuses, and
            // `reconcileOptimisticOverrides` drops the override once real
            // state agrees — including when the engine resolved the drop to a
            // reorder and the real column never changed.
            pendingMoveOriginByTaskID[taskID] = origin
            optimisticColumnByTaskID[taskID] = column
            invalidateWorkCache()
        }

        engine.sendMoveWorkItemOnBoard(id: taskID, column: column, group: group)
        return true
    }

    /// Which of its column's named groups `task` currently renders in, or
    /// `nil` for a column with no groups. Mirrors the Done-column split in
    /// `computeWorkSections`; kept here so the drop path and the section
    /// builder cannot drift.
    func boardGroup(for task: WorkTask) -> WorkBoardGroupKey? {
        guard effectiveBoardColumn(for: task) == .done else { return nil }
        return task.isInMergingSection ? .merging : .completed
    }

    func clearDragRefusal() {
        dragRefusalNotice = nil
    }

    func scheduleDragRefusalDismiss(for taskID: String) {
        Task { [weak self] in
            try? await Task.sleep(nanoseconds: 5_000_000_000)
            await MainActor.run { [weak self] in
                guard let self,
                      self.dragRefusalNotice?.taskID == taskID
                else { return }
                self.dragRefusalNotice = nil
            }
        }
    }

    /// Bounce all unconfirmed in-flight optimistic moves back to their origin
    /// columns and surface an inline notice. Called when `work_error` arrives
    /// while moves are pending, or when `workItemUpdated` reports an unexpected
    /// status (engine silently rejected the transition).
    func bounceBackOptimisticMoves(message: String?) {
        guard !pendingMoveOriginByTaskID.isEmpty else { return }
        let bouncedIDs = Array(pendingMoveOriginByTaskID.keys)
        for id in bouncedIDs {
            optimisticColumnByTaskID.removeValue(forKey: id)
            pendingMoveOriginByTaskID.removeValue(forKey: id)
        }
        invalidateWorkCache()
        if let firstID = bouncedIDs.first, let message {
            dragRefusalNotice = DragRefusalNotice(taskID: firstID, message: message)
            scheduleDragRefusalDismiss(for: firstID)
        }
    }

    /// After the engine's work tree arrives and `tasksByProjectID` reflects
    /// the latest status, clear optimistic overrides for cards whose real
    /// board column now matches the target. Safe to call before the next
    /// SwiftUI render — the cache is already stale, so the first re-read
    /// will see the real `boardColumn` value, which equals the override we
    /// just dropped, producing no visible change.
    func reconcileOptimisticOverrides(from tasks: [WorkTask]) {
        for task in tasks {
            guard optimisticColumnByTaskID[task.id] != nil else { continue }
            let realColumn = realEffectiveBoardColumn(for: task)
            if realColumn == optimisticColumnByTaskID[task.id] {
                // Real state now matches: drop the override, no flicker.
                optimisticColumnByTaskID.removeValue(forKey: task.id)
                pendingMoveOriginByTaskID.removeValue(forKey: task.id)
            }
            // If the real column doesn't match and the move is still pending
            // (pendingMoveOriginByTaskID has an entry), the work_error handler
            // will bounce it when the error arrives. Leave the override in
            // place so the card stays at the optimistic position while we wait.
        }
    }
}
