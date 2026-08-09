import Foundation

extension ChatViewModel {
    struct DragRefusalNotice: Equatable {
        let taskID: String
        let message: String
    }

    /// A drag-to-Doing waiting on its `EvaluateDispatchAdmission` reply.
    struct PendingDragAdmissionCheck: Equatable {
        let taskID: String
        let column: WorkBoardColumnKey
        let group: WorkBoardGroupKey?
    }

    /// A drag-to-Doing whose evaluation reported an active, overridable
    /// dispatch pause and no other blocker — bound by the confirmation
    /// alert. Carries everything needed to either send the confirmed
    /// override or bounce the card back on cancel.
    struct PauseOverrideConfirmation: Equatable {
        let taskID: String
        let taskName: String
        let column: WorkBoardColumnKey
        let group: WorkBoardGroupKey?
        let pauseReason: String
        let pausedSinceEpochS: UInt64?
        /// Non-overridable blockers force will NOT lift, for informational
        /// display only (`DispatchAdmission.blockers` minus the hard ones,
        /// which would have refused the drag outright before a
        /// confirmation was ever offered).
        let informationalBlockers: [DispatchAdmissionBlocker]

        /// Body text for the confirmation alert: the pause reason, plus
        /// any informational-only blockers force will not touch.
        var alertMessage: String {
            var lines = ["Dispatch is paused: \(pauseReason). Start \(taskName) without resuming dispatch?"]
            if !informationalBlockers.isEmpty {
                lines.append("")
                lines.append("Also present (unaffected by force): " + informationalBlockers.map(\.message).joined(separator: "; "))
            }
            return lines.joined(separator: "\n")
        }
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

        if !staysPut, column == .doing {
            // Entering Doing may request execution. Check dispatch
            // admission before sending the drop so a paused engine offers
            // a confirmation instead of either silently queuing the card
            // (today's behavior) or, once force exists, silently
            // dispatching around an operator's pause with no heads-up. See
            // `EvaluateDispatchAdmission` /
            // docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md.
            // A false-positive check (the drop turns out to be a reorder,
            // or the item is human-driven) is harmless — evaluation is
            // read-only and the reply handler just forwards the drop
            // unchanged when there is nothing to override.
            pendingDragAdmissionCheck = PendingDragAdmissionCheck(taskID: taskID, column: column, group: group)
            engine.sendEvaluateDispatchAdmission(workItemId: taskID)
            return true
        }

        engine.sendMoveWorkItemOnBoard(id: taskID, column: column, group: group)
        return true
    }

    /// Reply handler for `.dispatchAdmissionEvaluated`, called from
    /// `ChatViewModel+EventHandling`. Forwards the already-optimistically-
    /// placed drop, offers a pause-only confirmation, or bounces the card
    /// back — see `attemptDrop`'s Doing-column branch for what queued this.
    func handleDispatchAdmissionEvaluated(_ admission: DispatchAdmission) {
        guard let pending = pendingDragAdmissionCheck, pending.taskID == admission.workItemID else {
            // Stale reply (superseded by a later drag) or an evaluation the
            // app requested for some other reason — nothing to forward.
            return
        }
        pendingDragAdmissionCheck = nil

        // Mirrors `pause_bypass_decision`'s order on the engine side exactly
        // (coordinator/dispatch_admission.rs): a hard blocker refuses
        // outright regardless of pause state, checked BEFORE "is there even
        // a pause to override" — a dependency-gated or capped item must
        // bounce back even when dispatch isn't paused at all.
        let hardBlockers = admission.hardBlockers
        if !hardBlockers.isEmpty {
            bounceBackOptimisticMoves(message: hardBlockers.map(\.message).joined(separator: "; "))
            return
        }

        if !admission.pause.active {
            // Nothing to override — forward the drop exactly as the
            // non-Doing path already does.
            engine.sendMoveWorkItemOnBoard(id: pending.taskID, column: pending.column, group: pending.group)
            return
        }

        if !admission.pause.overridable {
            let reason = "dispatch is paused" + (admission.pause.reason.map { ": \($0)" } ?? "")
                + " — this pause is not overridable"
            bounceBackOptimisticMoves(message: reason)
            return
        }

        let taskName = task(withID: pending.taskID)?.name ?? pending.taskID
        pendingPauseOverrideConfirmation = PauseOverrideConfirmation(
            taskID: pending.taskID,
            taskName: taskName,
            column: pending.column,
            group: pending.group,
            pauseReason: admission.pause.reason ?? "no reason recorded",
            pausedSinceEpochS: admission.pause.pausedSinceEpochS,
            informationalBlockers: admission.blockers.filter { dispatchAdmissionInformationalOnlyBlockerCodes.contains($0.code) }
        )
    }

    /// Operator confirmed the pause-override dialog: send the drop with
    /// the confirmed bypass and the observed pause generation.
    func confirmPauseOverrideDrag() {
        guard let confirmation = pendingPauseOverrideConfirmation else { return }
        pendingPauseOverrideConfirmation = nil
        engine.sendMoveWorkItemOnBoard(
            id: confirmation.taskID,
            column: confirmation.column,
            group: confirmation.group,
            bypassDispatchPause: true,
            observedPauseSinceEpochS: confirmation.pausedSinceEpochS
        )
    }

    /// Operator cancelled the pause-override dialog: bounce the
    /// optimistically-placed card back to its origin column.
    func cancelPauseOverrideDrag() {
        guard pendingPauseOverrideConfirmation != nil else { return }
        pendingPauseOverrideConfirmation = nil
        bounceBackOptimisticMoves(message: nil)
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
