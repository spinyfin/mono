import Foundation
import os.log

/// Debug-only substitute for the engine's "board drop is a reorder" log
/// line, which stops firing once the client no longer sends same-column
/// drops to the engine at all. Silent in normal use.
private let kanbanSameColumnDropLog = Logger(
    subsystem: "dev.spinyfin.bossmacapp",
    category: "kanban-same-column-drop"
)

extension ChatViewModel {
    struct DragRefusalNotice: Equatable {
        let taskID: String
        let message: String
        /// Machine-readable engine reason (`bound_pr`, …); nil for refusals
        /// the client detected itself.
        var code: String? = nil
        /// Engine-suggested next step (`add_revision`), if any.
        var suggestedAction: String? = nil
    }

    /// A drag-to-Doing waiting on its `EvaluateDispatchAdmission` reply.
    struct PendingDragAdmissionCheck: Equatable {
        var requestID: String = UUID().uuidString
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
        guard canSubmitUserActions() else { return false }
        guard let task = task(withID: taskID) else { return false }
        let group = group.flatMap { $0.isEngineBoardGroup ? $0 : nil }

        // Whether the card visibly moved. This is a *rendering* fact the
        // client already owns (it is how the board lays sections out), not a
        // status decision. Kanban lanes are not user-ordered, so a drop that
        // leaves the card where it started carries no intent to record —
        // see the early return just below.
        let origin = effectiveBoardColumn(for: task)
        let originGroup = boardGroup(for: task)
        let staysPut = origin == column && (group == nil || originGroup == group)

        if staysPut {
            // Kanban lanes are not user-ordered, so a drop that lands back on
            // the card's own column (and group) carries no user intent to
            // record — nothing moved, nothing to send. Kept as a debug log
            // rather than silence: it is the client-side substitute for the
            // engine's now-unreachable "board drop is a reorder" log line,
            // which stops firing once the client never sends this drop.
            kanbanSameColumnDropLog.debug("same-column drop is a no-op: task=\(task.id, privacy: .public)")
            return true
        }

        // The engine owns dependency admission through `gating_prereqs_for`,
        // as for an explicit `RequestExecution`: an `in_review` prerequisite
        // satisfies a revision. Forward the drop; the engine answers a
        // refusal with `work_item_refused`, and `handleWorkItemRefused` shows
        // the reason on the card. Display helpers do not decide admission.

        // Moving out of Doing while a live worker is attached is allowed only
        // to Backlog (stop + park) or Done. The popover's Move buttons reach
        // this same check through `moveTask`.
        if origin == .doing,
           column != .backlog,
           column != .done,
           hasLiveWorker(forTaskID: taskID)
        {
            let message = "\(task.name) is being worked on by a live worker. Stop the worker before moving the card out of Doing."
            appendSystemMessage(message, alwaysShow: true)
            dragRefusalNotice = DragRefusalNotice(taskID: taskID, message: message)
            scheduleDragRefusalDismiss(for: taskID)
            return false
        }

        // Optimistic reposition: draw the card at the drop site until the
        // engine's answer arrives. `handleWorkItemRefused` returns it to
        // `origin` if the engine refuses, and `reconcileOptimisticOverrides`
        // drops the override once real state agrees.
        pendingMoveOriginByTaskID[taskID] = origin
        optimisticColumnByTaskID[taskID] = column
        invalidateWorkCache()

        if column == .doing, !task.humanDriven {
            // Entering Doing may request execution. Check dispatch
            // admission before sending the drop so a paused engine offers
            // a confirmation instead of either silently queuing the card
            // (today's behavior) or, once force exists, silently
            // dispatching around an operator's pause with no heads-up. See
            // `EvaluateDispatchAdmission` /
            // docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md.
            //
            // Human-driven items are excluded: `dispatch_admission_facts`
            // reports human-driven as a hard blocker (no agent worker will
            // ever run for it), but entering Doing without a worker is
            // this item's normal, fully-supported transition — see
            // `work_items.rs`'s "Human-driven rows enter Doing without a
            // worker". Routing it through the admission check would bounce
            // an ordinary drag on the hard-blocker check even with no
            // pause active at all, so it skips evaluation entirely and
            // goes straight to the normal drop below.
            let pending = PendingDragAdmissionCheck(taskID: taskID, column: column, group: group)
            pendingDragAdmissionChecks[taskID] = pending
            engine.sendEvaluateDispatchAdmission(workItemId: taskID, requestId: pending.requestID)
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
        guard let pending = pendingDragAdmissionChecks[admission.workItemID] else {
            engine.finishAdmissionRetry(taskID: admission.workItemID)
            // Evaluation the app requested for some other reason (or already
            // settled) — nothing to forward.
            return
        }
        pendingDragAdmissionChecks[admission.workItemID] = nil

        // Check "is there even a pause to override" FIRST. When there is
        // no active pause, this evaluation exists only to decide whether a
        // confirmation is needed — the engine's ordinary (non-bypass)
        // `RequestExecution` path already owns refusal for every other
        // reason (dependency gating, the interactive cap, an ineligible
        // status), exactly as it did before this evaluation existed. A
        // hard blocker must only ever suppress the confirmation dialog
        // when there is a pause it would otherwise be offered for — see
        // docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md,
        // "If there is no active pause, the drag follows the normal path
        // without an alert."
        if !admission.pause.active {
            // Nothing to override — forward the drop exactly as the
            // non-Doing path already does.
            engine.sendMoveWorkItemOnBoard(id: pending.taskID, column: pending.column, group: pending.group)
            return
        }

        // A pause IS active: mirrors `pause_bypass_decision`'s order on the
        // engine side (coordinator/dispatch_admission.rs) — a hard blocker
        // refuses outright regardless of the pause's overridability, since
        // force never lifts a hard blocker either.
        let hardBlockers = admission.hardBlockers
        if !hardBlockers.isEmpty {
            engine.finishAdmissionRetry(taskID: pending.taskID)
            bounceBackOptimisticMoves(message: hardBlockers.map(\.message).joined(separator: "; "), taskID: pending.taskID)
            return
        }

        if !admission.pause.overridable {
            engine.finishAdmissionRetry(taskID: pending.taskID)
            let reason = "dispatch is paused" + (admission.pause.reason.map { ": \($0)" } ?? "")
                + " — this pause is not overridable"
            bounceBackOptimisticMoves(message: reason, taskID: pending.taskID)
            return
        }

        let taskName = task(withID: pending.taskID)?.name ?? pending.taskID
        let confirmation = PauseOverrideConfirmation(
            taskID: pending.taskID,
            taskName: taskName,
            column: pending.column,
            group: pending.group,
            pauseReason: admission.pause.reason ?? "no reason recorded",
            pausedSinceEpochS: admission.pause.pausedSinceEpochS,
            informationalBlockers: admission.blockers.filter { dispatchAdmissionInformationalOnlyBlockerCodes.contains($0.code) }
        )
        if pendingPauseOverrideConfirmation == nil {
            pendingPauseOverrideConfirmation = confirmation
        } else {
            queuedPauseOverrideConfirmations.append(confirmation)
        }
    }

    /// Present the next queued confirmation, if any, once one is settled.
    private func presentNextPauseOverrideConfirmation() {
        pendingPauseOverrideConfirmation = queuedPauseOverrideConfirmations.isEmpty
            ? nil : queuedPauseOverrideConfirmations.removeFirst()
    }

    /// Operator confirmed the pause-override dialog: send the drop with
    /// the confirmed bypass and the observed pause generation.
    func confirmPauseOverrideDrag() {
        guard let confirmation = pendingPauseOverrideConfirmation else { return }
        presentNextPauseOverrideConfirmation()
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
        guard let confirmation = pendingPauseOverrideConfirmation else { return }
        engine.finishAdmissionRetry(taskID: confirmation.taskID)
        presentNextPauseOverrideConfirmation()
        bounceBackOptimisticMoves(message: nil, taskID: confirmation.taskID)
    }

    /// Which of its column's named groups `task` currently renders in, or
    /// `nil` for a column with no groups. Mirrors the Doing- and Done-column
    /// splits in `computeWorkSections`; kept here so the drop path and the
    /// section builder cannot drift.
    func boardGroup(for task: WorkTask) -> WorkBoardGroupKey? {
        switch effectiveBoardColumn(for: task) {
        case .doing:
            return task.isAwaitingOperatorAnswer ? .needsAttention : nil
        case .done:
            return task.isInMergingSection ? .merging : .completed
        default:
            return nil
        }
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

    /// Settles only drags that are not parked for Retry: a parked drag's
    /// request is still in the unsent queue and its continuation must
    /// survive until the user retries or discards it.
    func bounceUnparkedDrags(message: String?, includeMovesWithoutCheck: Bool = true) {
        let parked = engine.unsentDragTaskIDs()
        var ids = Set(pendingDragAdmissionChecks.keys)
        if includeMovesWithoutCheck { ids.formUnion(pendingMoveOriginByTaskID.keys) }
        ids.subtract(parked)
        guard !ids.isEmpty else { return }
        for id in ids { pendingDragAdmissionChecks[id] = nil }
        bounceBackOptimisticMoves(message: message, only: ids)
    }

    /// Bounce unconfirmed in-flight optimistic moves back to their origin
    /// columns and surface an inline notice. With a `taskID` (the engine
    /// named the card it refused) only that card is bounced — other cards'
    /// moves are still in flight and unaffected. Without one (an error that
    /// cannot be attributed to a card: `work_error` while moves are pending,
    /// or `workItemUpdated` reporting an unexpected status) every pending
    /// move is bounced, except drags parked for Retry unless explicitly
    /// selected with `only`.
    /// Returns whether any card was bounced, so a caller can fall back to a
    /// modal error when nothing was eligible (e.g. only parked drags pending).
    @discardableResult
    func bounceBackOptimisticMoves(
        message: String?,
        only: Set<String>? = nil,
        taskID: String? = nil,
        code: String? = nil,
        suggestedAction: String? = nil
    ) -> Bool {
        let bouncedIDs: [String]
        if let taskID {
            bouncedIDs = pendingMoveOriginByTaskID[taskID] != nil ? [taskID] : []
        } else {
            guard !pendingMoveOriginByTaskID.isEmpty else { return false }
            let selected = only ?? Set(pendingMoveOriginByTaskID.keys).subtracting(engine.unsentDragTaskIDs())
            bouncedIDs = pendingMoveOriginByTaskID.keys.filter { selected.contains($0) }
            guard !bouncedIDs.isEmpty else { return false }
        }
        for id in bouncedIDs {
            optimisticColumnByTaskID.removeValue(forKey: id)
            pendingMoveOriginByTaskID.removeValue(forKey: id)
        }
        if !bouncedIDs.isEmpty {
            invalidateWorkCache()
        }
        if let noticeID = taskID ?? bouncedIDs.first, let message {
            dragRefusalNotice = DragRefusalNotice(
                taskID: noticeID, message: message, code: code, suggestedAction: suggestedAction
            )
            scheduleDragRefusalDismiss(for: noticeID)
        }
        return !bouncedIDs.isEmpty
    }

    /// Settle one card's in-flight admission check as failed: drop the check
    /// and roll back only that card's optimistic placement.
    func bounceBackAdmissionCheck(taskID: String, message: String) {
        if pendingDragAdmissionChecks.removeValue(forKey: taskID) != nil {
            engine.finishAdmissionRetry(taskID: taskID)
        }
        bounceBackOptimisticMoves(message: message, taskID: taskID)
    }

    /// Reply handler for `.workItemRefused`: the engine refused or reverted a
    /// board gesture and says why. Bounces the optimistic placement back and
    /// shows the engine's reason on the dragged card (even when no optimistic
    /// move is pending), with the engine's suggested next step if any.
    func handleWorkItemRefused(
        id: String,
        code: String,
        message: String,
        suggestedAction: String?,
        requestId: String? = nil
    ) {
        if let requestId {
            reorderRequestTaskIDs.removeValue(forKey: requestId)
        }
        if pendingDragAdmissionChecks.removeValue(forKey: id) != nil {
            engine.finishAdmissionRetry(taskID: id)
        }
        bounceBackOptimisticMoves(message: message, taskID: id, code: code, suggestedAction: suggestedAction)
    }

    /// Follow the engine's suggested next step from a refusal notice.
    /// `add_revision` opens the revision composer for the refused card.
    func performDragRefusalAction(_ notice: DragRefusalNotice) {
        if notice.suggestedAction == "add_revision" {
            revisionComposerTaskID = notice.taskID
        }
        clearDragRefusal()
    }

    /// File the revision typed into the composer against its parent card.
    /// Blank text is ignored (the composer keeps its Create button disabled).
    func submitRevision(parentTaskID: String, description: String) {
        let trimmed = description.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        // A full retained-action ledger keeps the composer (and the draft it
        // alone holds) open instead of silently dropping the request.
        guard canSubmitUserActions(describing: "Add revision") else { return }
        let requestID = engine.sendCreateRevision(parentTaskId: parentTaskID, description: trimmed)
        // `nil` is either a retained request (saved for Retry — safe to close)
        // or a rejected one, which must not lose the draft.
        guard requestID != nil || engine.hasUnsentAction(kind: "create_revision") else { return }
        revisionComposerTaskID = nil
    }

    func cancelRevisionComposer() {
        revisionComposerTaskID = nil
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
