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
        openReviewGuide(versionId: versionId, rootTaskId: task.id)
        // The version on hand may be for an earlier head of the PR.
        requestReviewGuideOnOpen(for: task)
    }

    /// Open one guide version in the viewer window. `onOpen` is true for a
    /// person opening a guide, false for switching versions inside a viewer or
    /// retrying a failed load, which the engine does not count as opens.
    @MainActor
    func openReviewGuide(versionId: String, rootTaskId: String, onOpen: Bool = true) {
        pendingReviewGuideVersionId = versionId
        pendingReviewGuideRootTaskId = rootTaskId
        loadAttachments(taskId: rootTaskId)
        // Clear the design-doc / task-description identity guards for this
        // shared singleton window so a late reply for either cannot
        // overwrite the guide we are about to show.
        pendingAsyncViewerRef = nil
        asyncMarkdownViewerVM.clickStartTime = Date()
        asyncMarkdownViewerVM.collapsedByDefaultHeadings = []
        asyncMarkdownViewerVM.reviewGuideRootTaskId = rootTaskId
        asyncMarkdownViewerVM.reviewGuideGeneratedAt = nil
        asyncMarkdownViewerVM.reviewGuideComparisonId = nil
        asyncMarkdownViewerVM.state = .loading(title: reviewGuideViewerTitle(rootTaskId: rootTaskId))
        asyncMarkdownViewerVM.staleReason = nil
        asyncMarkdownViewerVM.canRetry = false
        asyncMarkdownViewerVM.onRetry = { [weak self] in
            if let self, let task = self.task(withID: rootTaskId) { self.retryReviewGuide(for: task) }
        }
        asyncMarkdownViewerVM.pendingRenderProjectShortID = nil
        asyncMarkdownViewerOpener?()
        pendingReviewGuideRequestId = engine.sendGetReviewGuideContent(versionID: versionId, onOpen: onOpen)
        if pendingReviewGuideRequestId == nil {
            asyncMarkdownViewerVM.state = .failed(title: "Review guide", message: "Guide request could not be sent. Reconnect and retry.")
            asyncMarkdownViewerVM.canRetry = true
            asyncMarkdownViewerVM.onRetry = { [weak self] in
                self?.openReviewGuide(versionId: versionId, rootTaskId: rootTaskId, onOpen: false)
            }
        }
    }

    /// Load a guide version into the navigator's inline reader. `onOpen` is
    /// true for the load that follows a person opening the Review tab; the
    /// engine counts it. A guide the reader already holds is not fetched
    /// again, so that open is reported with a fetch whose reply is not wanted.
    func loadNavigatorReviewGuide(for root: WorkTask, versionID: String? = nil, refresh: Bool = false, onOpen: Bool = false) {
        var fetched = false
        navigatorReviewGuides.load(root: root, versionID: versionID, refresh: refresh) {
            fetched = true
            return engine.sendGetReviewGuideContent(versionID: $0, onOpen: onOpen)
        }
        if onOpen, !fetched, hasCurrentReviewGuide(root),
           let heldVersionID = versionID ?? root.reviewGuideReadableVersionId {
            _ = engine.sendGetReviewGuideContent(versionID: heldVersionID, onOpen: true)
        }
    }

    /// Whether the task has a readable guide generated for its PR's current
    /// comparison. A guide for an earlier head of the PR is not current.
    func hasCurrentReviewGuide(_ task: WorkTask) -> Bool {
        task.reviewGuideReadableVersionId != nil && task.reviewGuideStaleSource != true
    }

    /// The navigator's Review tab came up for `root`. Returns whether this is a
    /// new open (the tab was not already showing this root). A new open asks the
    /// engine for a guide when there is none for the PR's current comparison.
    @MainActor
    func reviewGuideSurfaceOpened(root: WorkTask) -> Bool {
        guard reviewGuideSurfaceRootTaskID != root.id else { return false }
        reviewGuideSurfaceRootTaskID = root.id
        requestReviewGuideOnOpen(for: root)
        return true
    }

    @MainActor
    func reviewGuideSurfaceClosed(rootTaskID: String) {
        if reviewGuideSurfaceRootTaskID == rootTaskID { reviewGuideSurfaceRootTaskID = nil }
    }

    /// Generate-on-open: a person opening a PR's guide when none exists for its
    /// current comparison asks the engine for one, through the ordinary generate
    /// request marked as made on open. Whether the guide is made now, was
    /// already queued, or is held for the PR to be ready is the engine's
    /// business; the app does not know which experiment arm a PR is in. The
    /// engine runs one attempt per PR, so asking again while one is live is safe.
    ///
    /// Not asked for work that gets no automatic guide (design and
    /// investigation PRs), for a guide whose last attempt failed (Retry is on
    /// the card, and opening must not keep spending on a failing guide), or for
    /// a PR with no URL.
    @MainActor
    func requestReviewGuideOnOpen(for task: WorkTask) {
        guard task.generateReviewGuideMenuTitle != nil,
              !hasCurrentReviewGuide(task),
              !["design", "investigation"].contains(task.kind),
              task.reviewGuideLifecycle != "failed",
              !retryingReviewGuideRootTaskIDs.contains(task.id) else { return }
        retryingReviewGuideRootTaskIDs.insert(task.id)
        if engine.sendGenerateReviewGuide(rootTaskID: task.id, idempotencyToken: UUID().uuidString, onOpen: true) == nil {
            retryingReviewGuideRootTaskIDs.remove(task.id)
        }
    }

    /// Window title for a review guide, shared by the loading and loaded
    /// states so the title does not change while the content arrives.
    @MainActor
    func reviewGuideViewerTitle(rootTaskId: String?) -> String {
        rootTaskId.flatMap { task(withID: $0) }.map { "Review guide: \($0.name)" } ?? "Review guide"
    }

    /// Apply a `review_guide_content` reply. Dropped when `versionId` no
    /// longer matches the pending open (response-identity guard) — a
    /// stale/late response for a since-abandoned guide must never overwrite
    /// whatever the user has since opened in this shared window. Only a
    /// `nil` content for the CURRENTLY pending version counts as a real
    /// failure worth showing.
    @MainActor
    func applyReviewGuideContent(versionId: String, content: ReviewGuideVersionContent?, requestId: String? = nil) {
        guard pendingReviewGuideVersionId == versionId,
              requestId == nil || pendingReviewGuideRequestId == requestId else { return }
        pendingReviewGuideRequestId = nil
        guard let content else {
            asyncMarkdownViewerVM.state = .failed(
                title: "Review guide",
                message: "This review guide is no longer available."
            )
            return
        }
        let rootTaskId = pendingReviewGuideRootTaskId
        let title = reviewGuideViewerTitle(rootTaskId: rootTaskId)
        asyncMarkdownViewerVM.renderStartTime = Date()
        asyncMarkdownViewerVM.renderContentID = UUID()
        asyncMarkdownViewerVM.staleReason = nil
        asyncMarkdownViewerVM.canRetry = false
        asyncMarkdownViewerVM.reviewGuideGeneratedAt = content.generatedAt
        asyncMarkdownViewerVM.reviewGuideComparisonId = content.comparisonId
        asyncMarkdownViewerVM.state = .loaded(title: title, markdown: content.markdown, artifact: .reviewGuide(seriesID: content.seriesId, versionID: content.id))
    }

    /// Called directly from the card's generation menu action. Uses the same
    /// in-flight guard and queued/error replies as Retry.
    func generateReviewGuide(for task: WorkTask) {
        guard !retryingReviewGuideRootTaskIDs.contains(task.id) else { return }
        retryingReviewGuideRootTaskIDs.insert(task.id)
        if engine.sendGenerateReviewGuide(rootTaskID: task.id, idempotencyToken: UUID().uuidString) == nil {
            retryingReviewGuideRootTaskIDs.remove(task.id)
            showDisconnectedActionNotice()
        }
    }

    /// Ask the engine for another generation attempt. Guards against a
    /// duplicate tap while one is already in flight for this task.
    func retryReviewGuide(for task: WorkTask) {
        guard !retryingReviewGuideRootTaskIDs.contains(task.id) else { return }
        retryingReviewGuideRootTaskIDs.insert(task.id)
        if engine.sendRetryReviewGuide(rootTaskID: task.id, idempotencyToken: UUID().uuidString) == nil {
            retryingReviewGuideRootTaskIDs.remove(task.id)
            showDisconnectedActionNotice()
        }
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
