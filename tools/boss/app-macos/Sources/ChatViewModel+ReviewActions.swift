import Foundation

/// Merge-when-ready and review/live-workspace terminal actions.
extension ChatViewModel {
    /// Inline confirmation banner shown next to a card whose
    /// `merge_when_ready_accepted` reply just arrived (e.g. "Submitted to
    /// Trunk merge queue"), keyed by the wire `action` value in
    /// `ChatViewModel+EventHandling`. Single-slot and auto-dismissed —
    /// mirrors `dragRefusalNotice`.
    struct MergeFeedbackNotice: Equatable {
        let taskID: String
        let message: String
    }

    /// Window that initiated a merge-when-ready attempt. Recorded on
    /// `mergeWhenReady(for:origin:)` and copied onto the confirmation so
    /// each window's alert only presents the dialog it owns.
    enum MergeRevisionConfirmationOrigin: Equatable {
        case board
        case reviewGuideViewer
    }

    /// Set once `MergeConfirmationRequired` reports open revisions blocking
    /// an in-flight merge attempt — the confirmation dialog binds to this,
    /// mirroring `pendingPauseOverrideConfirmation`'s declarative shape.
    /// `origin` names the window that should present it. `nil` means no
    /// confirmation is showing.
    struct MergeRevisionConfirmation: Equatable {
        let workItemID: String
        let revisions: [OpenMergeRevision]
        let origin: MergeRevisionConfirmationOrigin

        var alertMessage: String {
            revisions.map { "ID \($0.label) — \($0.statusLabel)" }.joined(separator: "\n")
                + "\n\nThese revisions may still change this PR. Merge anyway?"
        }
    }

    /// Ask the engine to merge (or queue for merging) the PR for the given
    /// Review-column task. Guards against a duplicate tap while the RPC is
    /// in flight. The engine runs `gh pr merge --auto --squash` and kicks
    /// the PR-reconciler so the kanban state updates promptly on success.
    /// `origin` is the window whose merge control was used, so the
    /// open-revision confirmation can present on that window.
    func mergeWhenReady(for task: WorkTask, origin: MergeRevisionConfirmationOrigin = .board) {
        guard let prURL = task.prURL, !prURL.isEmpty else { return }
        _ = prURL  // consumed by the engine; kept here for the guard above
        guard !mergingWhenReadyIDs.contains(task.id) else { return }
        // A prior failure banner must not survive a fresh attempt — otherwise
        // a later accept shows the green notice for five seconds and then
        // falls through to the stale error for the rest of the session.
        mergeErrorNoticesByTaskID.removeValue(forKey: task.id)
        mergingWhenReadyIDs.insert(task.id)
        mergeRevisionConfirmationOrigins[task.id] = origin
        engine.sendMergeWhenReady(workItemID: task.id)
    }

    /// If a different task's confirmation is already showing, queue this one
    /// rather than overwriting `pendingMergeRevisionConfirmation` — an
    /// overwrite would silently strand the first task's dialog: its entry in
    /// `mergingWhenReadyIDs` would never clear because neither
    /// `confirmMergeRevision(workItemID:)` nor
    /// `cancelMergeRevisionConfirmation(workItemID:)` would ever run for it.
    func handleMergeConfirmation(workItemID: String, revisions: [OpenMergeRevision]) {
        guard mergingWhenReadyIDs.contains(workItemID) else { return }
        let origin = mergeRevisionConfirmationOrigins[workItemID] ?? .board
        let confirmation = MergeRevisionConfirmation(
            workItemID: workItemID, revisions: revisions, origin: origin
        )
        if pendingMergeRevisionConfirmation == nil {
            pendingMergeRevisionConfirmation = confirmation
        } else if pendingMergeRevisionConfirmation?.workItemID == workItemID {
            // A repeat confirmation for the task already showing — refresh it.
            pendingMergeRevisionConfirmation = confirmation
        } else {
            queuedMergeRevisionConfirmations.removeAll { $0.workItemID == workItemID }
            queuedMergeRevisionConfirmations.append(confirmation)
        }
    }

    /// Whether `surface`'s alert should be on screen. Viewer-origin
    /// confirmations present on the viewer while it is open, and fall back
    /// to the board when it is not, so a confirmation always has a window.
    func shouldPresentMergeRevisionConfirmation(on surface: MergeRevisionConfirmationOrigin) -> Bool {
        guard let pending = pendingMergeRevisionConfirmation else { return false }
        return presentingSurface(for: pending) == surface
    }

    /// The pending confirmation if `surface` is the one that should show it.
    func mergeRevisionConfirmation(for surface: MergeRevisionConfirmationOrigin) -> MergeRevisionConfirmation? {
        guard shouldPresentMergeRevisionConfirmation(on: surface) else { return nil }
        return pendingMergeRevisionConfirmation
    }

    /// User confirmed merging while revisions are open: resend the merge
    /// request with the confirmed revisions attached. Ignored when
    /// `workItemID` is not the currently pending confirmation — a stale
    /// click from another window must not confirm a different task.
    func confirmMergeRevision(workItemID: String) {
        guard let confirmation = pendingMergeRevisionConfirmation,
              confirmation.workItemID == workItemID else { return }
        pendingMergeRevisionConfirmation = nil
        engine.sendMergeWhenReady(workItemID: confirmation.workItemID, confirmedRevisions: confirmation.revisions)
        presentNextQueuedMergeRevisionConfirmationIfNeeded()
    }

    /// User declined merging while revisions are open: drop the in-flight
    /// guard so a fresh attempt can be made later. Ignored when
    /// `workItemID` is not the currently pending confirmation.
    func cancelMergeRevisionConfirmation(workItemID: String) {
        guard let confirmation = pendingMergeRevisionConfirmation,
              confirmation.workItemID == workItemID else { return }
        pendingMergeRevisionConfirmation = nil
        mergingWhenReadyIDs.remove(confirmation.workItemID)
        mergeRevisionConfirmationOrigins.removeValue(forKey: confirmation.workItemID)
        presentNextQueuedMergeRevisionConfirmationIfNeeded()
    }

    /// Presents the next queued confirmation (if any) on the next runloop
    /// tick, after the current one has already cleared. Deferring is
    /// required for the alert to re-present at all: SwiftUI's
    /// `isPresented`/`presenting:` alert only triggers on a genuine
    /// false→true transition, so setting the property directly to the next
    /// value in the same call as clearing the old one would never surface a
    /// second dialog.
    ///
    /// The queued item stays in the array until this callback runs. If
    /// `handleMergeConfirmation` installs a new pending confirmation in
    /// between, the callback leaves the queue untouched rather than
    /// overwriting that newer pending item (which would strand its
    /// `mergingWhenReadyIDs` slot). Entries whose in-flight guard has
    /// since been cleared (e.g. by a `workError`) are dropped.
    private func presentNextQueuedMergeRevisionConfirmationIfNeeded() {
        guard pendingMergeRevisionConfirmation == nil, !queuedMergeRevisionConfirmations.isEmpty else { return }
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            guard self.pendingMergeRevisionConfirmation == nil else { return }
            self.queuedMergeRevisionConfirmations.removeAll {
                !self.mergingWhenReadyIDs.contains($0.workItemID)
            }
            guard !self.queuedMergeRevisionConfirmations.isEmpty else { return }
            self.pendingMergeRevisionConfirmation = self.queuedMergeRevisionConfirmations.removeFirst()
        }
    }

    private func presentingSurface(for confirmation: MergeRevisionConfirmation) -> MergeRevisionConfirmationOrigin {
        if confirmation.origin == .reviewGuideViewer, isReviewGuideViewerWindowOpen {
            return .reviewGuideViewer
        }
        return .board
    }

    /// Ask the engine to lease a workspace for the given Review-column
    /// task's PR branch and open a terminal there. Opens the window
    /// immediately with a loading spinner; the terminal becomes live once
    /// the engine sends back `ReviewTerminalReady`.
    func openReviewTerminal(for task: WorkTask) {
        guard let prURL = task.prURL, !prURL.isEmpty else { return }
        guard !openingReviewTerminalIDs.contains(task.id) else {
            // Same task still loading — just re-focus the window.
            reviewTerminalOpener?()
            return
        }
        reviewTerminalVM.state = .loading(taskName: task.name)
        reviewTerminalOpener?()
        openingReviewTerminalIDs.insert(task.id)
        engine.sendOpenReviewTerminal(workItemID: task.id)
    }

    /// Notify the engine that the review terminal for `leaseID` has
    /// closed so the workspace lease can be released. Called from the
    /// `ReviewTerminalView.onDisappear` handler.
    func releaseReviewTerminal(leaseID: String) {
        engine.sendReleaseReviewTerminal(leaseID: leaseID)
    }

    /// Ask the engine for a terminal into a Doing-column task's already-
    /// live execution workspace — no new lease, just the path the running
    /// worker is already using. Opens the same window as
    /// `openReviewTerminal` with a loading spinner; becomes live once the
    /// engine sends back `LiveWorkspaceTerminalReady`. Unlike the review
    /// flow, the window's `onDisappear` never releases a lease, since the
    /// worker owns it for the lifetime of its run.
    func openLiveWorkspaceTerminal(for task: WorkTask) {
        guard !openingLiveWorkspaceTerminalIDs.contains(task.id) else {
            // Same task still loading — just re-focus the window.
            reviewTerminalOpener?()
            return
        }
        reviewTerminalVM.state = .loading(taskName: task.name)
        reviewTerminalOpener?()
        openingLiveWorkspaceTerminalIDs.insert(task.id)
        engine.sendOpenLiveWorkspaceTerminal(workItemID: task.id)
    }
}
