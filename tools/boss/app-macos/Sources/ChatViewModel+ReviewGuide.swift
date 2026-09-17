import Foundation

/// Opening PR review guides in the shared async markdown viewer, and the
/// Review-card Retry affordance. Mirrors `ChatViewModel+DesignDocs.swift`'s
/// `openDesignDocViaEngine` / `applyContentToAsyncViewerIfPending` shape,
/// with the immutable version id (rather than a repo/path/ref triple) as
/// the response-identity key. See
/// `tools/boss/docs/designs/automatic-pr-review-guides.md`, "Review card
/// and viewer".
extension ChatViewModel {
    /// Open a task's current readable review-guide version in the async
    /// markdown viewer. No-op when the task has no readable version yet —
    /// the card only ever shows the document button when one exists, so
    /// this should not be reachable without one, but a live task can race
    /// the click (e.g. a concurrent history change clears the pointer).
    @MainActor
    func openReviewGuide(for task: WorkTask) {
        guard let versionId = task.reviewGuideReadableVersionId else { return }
        pendingReviewGuideVersionId = versionId
        pendingReviewGuideRootTaskId = task.id
        // Clear the design-doc / task-description identity guards for this
        // shared singleton window so a late reply for either cannot
        // overwrite the guide we are about to show.
        pendingAsyncViewerRef = nil
        asyncMarkdownViewerVM.clickStartTime = Date()
        asyncMarkdownViewerVM.collapsedByDefaultHeadings = []
        asyncMarkdownViewerVM.reviewGuideRootTaskId = task.id
        asyncMarkdownViewerVM.reviewGuideGeneratedAt = nil
        asyncMarkdownViewerVM.reviewGuideComparisonId = nil
        asyncMarkdownViewerVM.state = .loading
        asyncMarkdownViewerVM.staleReason = nil
        asyncMarkdownViewerVM.canRetry = false
        asyncMarkdownViewerVM.onRetry = { [weak self] in
            self?.retryReviewGuide(for: task)
        }
        asyncMarkdownViewerVM.pendingRenderProjectShortID = nil
        asyncMarkdownViewerOpener?()
        engine.sendGetReviewGuideContent(versionID: versionId)
    }

    /// Apply a `review_guide_content` reply. Dropped when `versionId` no
    /// longer matches the pending open (response-identity guard) — a
    /// stale/late response for a since-abandoned guide must never overwrite
    /// whatever the user has since opened in this shared window. Only a
    /// `nil` content for the CURRENTLY pending version counts as a real
    /// failure worth showing.
    @MainActor
    func applyReviewGuideContent(versionId: String, content: ReviewGuideVersionContent?) {
        guard pendingReviewGuideVersionId == versionId else { return }
        guard let content else {
            asyncMarkdownViewerVM.state = .failed(
                title: "Review guide",
                message: "This review guide is no longer available."
            )
            return
        }
        let rootTaskId = pendingReviewGuideRootTaskId
        let title = rootTaskId.flatMap { task(withID: $0) }.map { "Review guide: \($0.name)" } ?? "Review guide"
        asyncMarkdownViewerVM.renderStartTime = Date()
        asyncMarkdownViewerVM.renderContentID = UUID()
        asyncMarkdownViewerVM.staleReason = nil
        asyncMarkdownViewerVM.canRetry = false
        asyncMarkdownViewerVM.reviewGuideGeneratedAt = content.generatedAt
        asyncMarkdownViewerVM.reviewGuideComparisonId = content.comparisonId
        asyncMarkdownViewerVM.state = .loaded(title: title, markdown: content.markdown, artifact: nil)
    }

    /// Ask the engine for another generation attempt. Guards against a
    /// duplicate tap while one is already in flight for this task.
    func retryReviewGuide(for task: WorkTask) {
        guard !retryingReviewGuideRootTaskIDs.contains(task.id) else { return }
        retryingReviewGuideRootTaskIDs.insert(task.id)
        engine.sendRetryReviewGuide(rootTaskID: task.id, idempotencyToken: UUID().uuidString)
    }

    /// Apply a `review_guide_retry_queued` reply — clears the in-flight
    /// guard for the echoed task. The card's own state
    /// (`reviewGuideLifecycle` / `reviewGuideReadableVersionId`) updates
    /// through the normal task event pipeline once the engine's broadcast
    /// lands, not from this reply directly.
    func applyReviewGuideRetryQueued(rootTaskId: String) {
        retryingReviewGuideRootTaskIDs.remove(rootTaskId)
    }
}
