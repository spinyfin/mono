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

    /// Ask the engine to merge (or queue for merging) the PR for the given
    /// Review-column task. Guards against a duplicate tap while the RPC is
    /// in flight. The engine runs `gh pr merge --auto --squash` and kicks
    /// the PR-reconciler so the kanban state updates promptly on success.
    func mergeWhenReady(for task: WorkTask) {
        guard let prURL = task.prURL, !prURL.isEmpty else { return }
        _ = prURL  // consumed by the engine; kept here for the guard above
        guard !mergingWhenReadyIDs.contains(task.id) else { return }
        mergingWhenReadyIDs.insert(task.id)
        engine.sendMergeWhenReady(workItemID: task.id)
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
