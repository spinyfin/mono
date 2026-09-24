import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// Covers the PR review-guide viewer-open flow — the response-identity
/// guard, old-content retention, and Retry in-flight guard — plus isolated
/// renders of the card/popover badge across its five brief states. See
/// `tools/boss/docs/designs/automatic-pr-review-guides.md`, "Review card
/// and viewer".
@MainActor
final class ReviewGuideTests: XCTestCase {

    // MARK: - Opening

    func testGenerateMenuVisibilityAndLabelIncludingDoneWork() {
        var task = Self.makeTask(id: "task_1", readableVersionId: nil)
        for url: String? in [nil, "", " "] {
            task.prURL = url
            XCTAssertNil(task.generateReviewGuideMenuTitle)
        }
        task.prURL = "https://github.com/acme/widget/pull/9"
        for status in ["in_review", "done"] {
            task.status = status
            task.reviewGuideReadableVersionId = nil
            XCTAssertEqual(task.generateReviewGuideMenuTitle, "Generate Review Guide…")
            task.reviewGuideReadableVersionId = "old-guide"
            XCTAssertEqual(task.generateReviewGuideMenuTitle, "Regenerate Review Guide…")
        }
    }

    func testRevisionCardDoesNotOfferGeneration() {
        let task = Self.makeTask(id: "revision", readableVersionId: nil, kind: "revision")
        XCTAssertNotNil(task.prURL)
        XCTAssertNil(task.generateReviewGuideMenuTitle)
    }

    func testCaptureProgressUsesInFlightStateAndClearsOnError() {
        let model = makeModel()
        var task = Self.makeTask(id: "root", readableVersionId: nil)
        task.reviewGuideLifecycle = nil
        func snapshot(_ column: WorkBoardColumnKey) -> WorkCardSnapshot {
            model.workCardSnapshot(
                for: task, column: column, isSelected: false, isFrontierHighlighted: false,
                boardStyle: .classic, liveState: nil
            )
        }
        XCTAssertNil(snapshot(.review).reviewGuidePresentation)
        model.retryingReviewGuideRootTaskIDs.insert(task.id)
        XCTAssertEqual(snapshot(.review).reviewGuidePresentation?.kind, .generating)
        XCTAssertEqual(snapshot(.done).reviewGuidePresentation?.kind, .generating)
        task.reviewGuideReadableVersionId = "old"
        XCTAssertEqual(snapshot(.review).reviewGuidePresentation?.kind, .refreshing)
        XCTAssertEqual(snapshot(.review).reviewGuidePresentation?.readableVersionId, "old")
        model.applyEventForTest(.workError(message: "capture failed", requestId: "request"))
        XCTAssertNil(snapshot(.review).reviewGuidePresentation)
    }

    func testGeneratingPresentationKeepsProgressAndPriorDocument() {
        let initial = ReviewGuideCardPresentation.from(lifecycle: "generating", readableVersionId: nil)
        XCTAssertEqual(initial?.kind, .generating)
        XCTAssertEqual(initial?.showsProgress, true)
        XCTAssertEqual(initial?.showsDocumentButton, false)
        let refresh = ReviewGuideCardPresentation.from(lifecycle: "generating", readableVersionId: "old")
        XCTAssertEqual(refresh?.kind, .refreshing)
        XCTAssertEqual(refresh?.showsProgress, true)
        XCTAssertEqual(refresh?.showsDocumentButton, true)
        XCTAssertEqual(refresh?.readableVersionId, "old")
        let viewer = ReviewGuideViewerCurrentness.from(
            lifecycle: "generating", readableVersionId: "old", selectedComparisonId: "comparison",
            displayedVersionId: "old", displayedComparisonId: "comparison"
        )
        XCTAssertEqual(viewer.status, .refreshing)
    }

    func testGenerateUsesWireRequestAndSurfacesDisconnectedError() {
        let model = makeModel()
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }
        let task = Self.makeTask(id: "task_1", readableVersionId: nil)
        model.generateReviewGuide(for: task)
        XCTAssertEqual(sent.count, 1)
        XCTAssertEqual(sent[0]["type"] as? String, "generate_review_guide")
        XCTAssertEqual(sent[0]["root_task_id"] as? String, task.id)
        XCTAssertNotNil(UUID(uuidString: sent[0]["idempotency_token"] as? String ?? ""))
        XCTAssertNotNil(model.workErrorMessage)
        XCTAssertFalse(model.retryingReviewGuideRootTaskIDs.contains(task.id))
    }

    func testGenerateSharesRetryGuardAndEngineErrorsAreVisible() {
        let model = makeModel()
        var sent = 0
        model.engine.outboundRecorder = { _ in sent += 1 }
        let task = Self.makeTask(id: "task_1", readableVersionId: nil)
        model.retryingReviewGuideRootTaskIDs.insert(task.id)
        model.generateReviewGuide(for: task)
        XCTAssertEqual(sent, 0)
        model.applyEventForTest(.workError(message: "source access refused", requestId: "request"))
        XCTAssertEqual(model.workErrorMessage, "source access refused")
        XCTAssertFalse(model.retryingReviewGuideRootTaskIDs.contains(task.id))
    }

    func testOpenReviewGuideNoOpWithoutReadableVersion() {
        let model = makeModel()
        let task = Self.makeTask(id: "task_1", readableVersionId: nil)

        model.openReviewGuide(for: task)

        XCTAssertNil(model.pendingReviewGuideVersionId)
        XCTAssertNil(model.pendingReviewGuideRootTaskId)
        guard case .loading = model.asyncMarkdownViewerVM.state else {
            return XCTFail("no readable version: viewer must not be disturbed")
        }
    }

    func testOpenReviewGuideSetsLoadingAndPendingIdentity() {
        let model = makeModel()
        let task = Self.makeTask(id: "task_1", readableVersionId: "prgv_1")

        model.openReviewGuide(for: task)

        XCTAssertEqual(model.pendingReviewGuideVersionId, "prgv_1")
        XCTAssertEqual(model.pendingReviewGuideRootTaskId, "task_1")
        XCTAssertNil(model.pendingReviewGuideRequestId, "headless tests have no engine connection, so sendLine returns nil")
        XCTAssertEqual(model.asyncMarkdownViewerVM.reviewGuideRootTaskId, "task_1")
        guard case .loading = model.asyncMarkdownViewerVM.state else {
            return XCTFail("expected .loading immediately on open")
        }
    }

    /// Opening a design doc afterward must not leave a stale review-guide
    /// pending identity around to falsely match a later reply.
    func testOpenDesignDocClearsReviewGuidePendingIdentity() {
        let model = makeModel()
        model.openReviewGuide(for: Self.makeTask(id: "task_1", readableVersionId: "prgv_1"))
        XCTAssertNotNil(model.pendingReviewGuideVersionId)

        model.openDesignDocViaEngine(
            ref: DesignDocRef(repoRemoteURL: "https://github.com/x/y", path: "docs/a.md", gitRef: "main"),
            title: "A doc",
            artifact: nil,
            projectShortID: "1"
        )

        XCTAssertNil(model.pendingReviewGuideVersionId)
        XCTAssertNil(model.pendingReviewGuideRequestId)
        XCTAssertNil(model.asyncMarkdownViewerVM.reviewGuideRootTaskId)
    }

    // MARK: - Response-identity guard / old-content retention

    /// A late reply for a version the user has since navigated away from
    /// (a second `openReviewGuide` for a different version) must not
    /// overwrite the now-current open — the shared singleton's core
    /// invariant.
    func testApplyReviewGuideContentDropsReplyForAbandonedVersion() {
        let model = makeModel()
        model.openReviewGuide(for: Self.makeTask(id: "task_1", readableVersionId: "prgv_old"))
        model.openReviewGuide(for: Self.makeTask(id: "task_2", readableVersionId: "prgv_new"))

        // The late reply for the abandoned "prgv_old" open.
        model.applyReviewGuideContent(
            versionId: "prgv_old",
            content: ReviewGuideVersionContent(
                id: "prgv_old",
                seriesId: "prgs_1",
                comparisonId: "prgc_1",
                attemptId: "prga_1",
                markdown: "# Old guide",
                contentHash: "hash-old",
                promptVersion: "review-guide-v1",
                generatedAt: "2026-09-01T00:00:00Z"
            )
        )

        guard case .loading = model.asyncMarkdownViewerVM.state else {
            return XCTFail("stale reply for an abandoned version must not touch viewer state")
        }
    }

    func testApplyReviewGuideContentAppliesMatchingReply() {
        let model = makeModel()
        model.taskIndexByID = ["task_1": Self.makeTask(id: "task_1", readableVersionId: "prgv_1", name: "Fix retry")]
        model.openReviewGuide(for: model.taskIndexByID!["task_1"]!)

        model.applyReviewGuideContent(
            versionId: "prgv_1",
            content: ReviewGuideVersionContent(
                id: "prgv_1",
                seriesId: "prgs_1",
                comparisonId: "prgc_1",
                attemptId: "prga_1",
                markdown: "# The guide",
                contentHash: "hash-1",
                promptVersion: "review-guide-v1",
                generatedAt: "2026-09-16T00:00:00Z"
            )
        )

        guard case .loaded(let title, let markdown, let artifact) = model.asyncMarkdownViewerVM.state else {
            return XCTFail("matching reply must load")
        }
        XCTAssertEqual(title, "Review guide: Fix retry")
        XCTAssertEqual(markdown, "# The guide")
        XCTAssertEqual(artifact, .reviewGuide(seriesID: "prgs_1", versionID: "prgv_1"))
        XCTAssertEqual(model.asyncMarkdownViewerVM.reviewGuideGeneratedAt, "2026-09-16T00:00:00Z")
    }

    func testApplyReviewGuideContentNilShowsFailed() {
        let model = makeModel()
        model.openReviewGuide(for: Self.makeTask(id: "task_1", readableVersionId: "prgv_1"))

        model.applyReviewGuideContent(versionId: "prgv_1", content: nil)

        guard case .failed = model.asyncMarkdownViewerVM.state else {
            return XCTFail("a nil content reply for the pending version must surface as failed")
        }
    }

    // MARK: - Retry in-flight guard

    func testRetryReviewGuideSetsInFlightGuard() {
        let model = makeModel()
        let task = Self.makeTask(id: "task_1", readableVersionId: nil)
        model.retryReviewGuide(for: task)
        XCTAssertTrue(model.retryingReviewGuideRootTaskIDs.contains("task_1"))
    }

    func testApplyReviewGuideRetryQueuedClearsGuardForEchoedTask() {
        let model = makeModel()
        model.retryingReviewGuideRootTaskIDs.insert("task_1")
        model.retryingReviewGuideRootTaskIDs.insert("task_2")

        model.applyReviewGuideRetryQueued(rootTaskId: "task_1")

        XCTAssertFalse(model.retryingReviewGuideRootTaskIDs.contains("task_1"))
        XCTAssertTrue(model.retryingReviewGuideRootTaskIDs.contains("task_2"), "unrelated in-flight guards must survive")
    }

    /// A disconnect while a retry request is in flight can never receive
    /// `review_guide_retry_queued` or `work_error` — without clearing the
    /// guard here, Retry becomes a permanent no-op for that task for the
    /// rest of the session.
    func testDisconnectClearsRetryInFlightGuard() {
        let model = makeModel()
        model.retryingReviewGuideRootTaskIDs.insert("task_1")

        model.applyEventForTest(.disconnected)

        XCTAssertTrue(model.retryingReviewGuideRootTaskIDs.isEmpty)
    }

    /// A disconnect while the async viewer is `.loading` a guide's content
    /// can equally never receive a reply — the viewer must fail with a
    /// retry action rather than spin forever.
    func testDisconnectFailsAViewerStuckLoadingGuideContent() {
        let model = makeModel()
        let task = Self.makeTask(id: "task_1", readableVersionId: "prgv_1")
        model.taskIndexByID = [task.id: task]
        model.openReviewGuide(for: task)
        guard case .loading = model.asyncMarkdownViewerVM.state else {
            return XCTFail("expected .loading immediately after open")
        }

        model.applyEventForTest(.disconnected)

        guard case .failed = model.asyncMarkdownViewerVM.state else {
            return XCTFail("a request that can never complete must fail, not stay loading forever")
        }
        XCTAssertTrue(model.asyncMarkdownViewerVM.canRetry)
        model.asyncMarkdownViewerVM.onRetry?()
        XCTAssertTrue(
            model.retryingReviewGuideRootTaskIDs.isEmpty,
            "retry must re-fetch content, not enqueue a new generation"
        )
        guard case .loading = model.asyncMarkdownViewerVM.state else {
            return XCTFail("retry must re-open the pending version")
        }
    }

    /// A `WorkError` while the async viewer is `.loading` a guide's content
    /// is the engine's actual reply when the version lookup fails — the
    /// viewer must fail with a retry that re-opens (re-fetches) the guide,
    /// not stay spinning, and not count as an untracked request that
    /// paints the raw message onto an unrelated attachments viewer.
    func testWorkErrorFailsAViewerStuckLoadingGuideContent() {
        let model = makeModel()
        let task = Self.makeTask(id: "task_1", readableVersionId: "prgv_1")
        model.taskIndexByID = [task.id: task]
        model.openReviewGuide(for: task)
        guard case .loading = model.asyncMarkdownViewerVM.state else {
            return XCTFail("expected .loading immediately after open")
        }
        model.pendingReviewGuideRequestId = "req_guide"
        model.attachmentsInFlightTaskIDs.insert("task_2")

        model.applyEventForTest(.workError(message: "no such version", requestId: "req_guide"))

        guard case .failed(title: _, message: let message) = model.asyncMarkdownViewerVM.state else {
            return XCTFail("a content-fetch WorkError must fail the loading viewer")
        }
        XCTAssertEqual(message, "no such version")
        XCTAssertTrue(model.asyncMarkdownViewerVM.canRetry)
        XCTAssertNil(model.pendingReviewGuideRequestId)
        XCTAssertEqual(
            model.attachmentsLoadFailureByTaskID["task_2"], "Loading failed. Retry?",
            "an in-flight guide fetch must count as another tracked request"
        )
        model.asyncMarkdownViewerVM.onRetry?()
        XCTAssertTrue(
            model.retryingReviewGuideRootTaskIDs.isEmpty,
            "retry must re-fetch content, not enqueue a new generation"
        )
        guard case .loading = model.asyncMarkdownViewerVM.state else {
            return XCTFail("retry must re-open the pending version")
        }
    }

    /// A WorkError for an abandoned guide must not fail the guide the
    /// user has since switched to.
    func testWorkErrorForAbandonedGuideDoesNotFailCurrentViewer() {
        let model = makeModel()
        let taskA = Self.makeTask(id: "task_a", readableVersionId: "prgv_a")
        let taskB = Self.makeTask(id: "task_b", readableVersionId: "prgv_b")
        model.taskIndexByID = [taskA.id: taskA, taskB.id: taskB]
        model.openReviewGuide(for: taskA)
        model.pendingReviewGuideRequestId = "req_a"
        model.openReviewGuide(for: taskB)
        model.pendingReviewGuideRequestId = "req_b"
        guard case .loading = model.asyncMarkdownViewerVM.state else {
            return XCTFail("expected .loading for the current open")
        }

        model.applyEventForTest(.workError(message: "no such version", requestId: "req_a"))

        guard case .loading = model.asyncMarkdownViewerVM.state else {
            return XCTFail("an abandoned guide's WorkError must not fail the current viewer")
        }
        XCTAssertEqual(model.pendingReviewGuideRequestId, "req_b")
        XCTAssertEqual(model.pendingReviewGuideVersionId, "prgv_b")
    }

    /// A WorkError for an unrelated in-flight request must not paint its
    /// message into a loading review-guide viewer.
    func testUnrelatedWorkErrorDoesNotFailLoadingGuideViewer() {
        let model = makeModel()
        let task = Self.makeTask(id: "task_1", readableVersionId: "prgv_1")
        model.taskIndexByID = [task.id: task]
        model.openReviewGuide(for: task)
        model.pendingReviewGuideRequestId = "req_guide"
        model.mergingWhenReadyIDs.insert("task_other")

        model.applyEventForTest(.workError(message: "merge queue failed", requestId: "req_merge"))

        guard case .loading = model.asyncMarkdownViewerVM.state else {
            return XCTFail("an unrelated WorkError must not fail the loading guide viewer")
        }
        XCTAssertEqual(model.pendingReviewGuideRequestId, "req_guide")
        XCTAssertEqual(
            model.mergeErrorNoticesByTaskID["task_other"], "merge queue failed",
            "the merge path still records its own failure"
        )
    }

    /// Keep version A open; version B publishes for a newer comparison;
    /// a retry on B's comparison then fails. The task-level stale flag
    /// is false (B matches the series' selected comparison) and lifecycle
    /// is `"failed"`, but the viewer is still showing A — it must offer
    /// the published B and still describe A as covering an older revision.
    func testViewerCurrentnessWhenOpenVersionLagsPublishedThenRetryFails() {
        let currentness = ReviewGuideViewerCurrentness.from(
            lifecycle: "failed",
            readableVersionId: "prgv_B",
            selectedComparisonId: "prgc_2",
            displayedVersionId: "prgv_A",
            displayedComparisonId: "prgc_1"
        )
        XCTAssertTrue(
            currentness.showsOpenUpdatedGuide,
            "a different readable version must be offered even when the latest attempt failed"
        )
        guard case .refreshFailed(let displayedStaleSource) = currentness.status else {
            return XCTFail("latest attempt failed, so the viewer still surfaces that failure")
        }
        XCTAssertTrue(
            displayedStaleSource,
            "displayed version A's comparison is not the series' current comparison"
        )
    }

    func testApplyReviewGuideContentRetainsDisplayedComparisonIdentity() {
        let model = makeModel()
        model.taskIndexByID = ["task_1": Self.makeTask(id: "task_1", readableVersionId: "prgv_1")]
        model.openReviewGuide(for: model.taskIndexByID!["task_1"]!)
        XCTAssertNil(model.asyncMarkdownViewerVM.reviewGuideComparisonId)

        model.applyReviewGuideContent(
            versionId: "prgv_1",
            content: ReviewGuideVersionContent(
                id: "prgv_1",
                seriesId: "prgs_1",
                comparisonId: "prgc_1",
                attemptId: "prga_1",
                markdown: "# The guide",
                contentHash: "hash-1",
                promptVersion: "review-guide-v1",
                generatedAt: "2026-09-16T00:00:00Z"
            )
        )

        XCTAssertEqual(model.asyncMarkdownViewerVM.reviewGuideComparisonId, "prgc_1")
    }

    /// `hasOtherTrackedAppRequestInFlight` exists precisely so an ambiguous
    /// `WorkError` is not painted onto the transcript/attachment viewers —
    /// a review-guide retry in flight must count as "some other tracked
    /// request", the same as every sibling in-flight set.
    func testWorkErrorWhileRetryInFlightIsNotAttributedToAttachmentsViewer() {
        let model = makeModel()
        model.retryingReviewGuideRootTaskIDs.insert("task_1")
        model.attachmentsInFlightTaskIDs.insert("task_2")

        model.applyEventForTest(.workError(message: "some ambiguous failure", requestId: nil))

        XCTAssertEqual(
            model.attachmentsLoadFailureByTaskID["task_2"], "Loading failed. Retry?",
            "the retry in flight should make the failure ambiguous, not attributed to the attachments viewer"
        )
    }

    // MARK: - Card badge: isolated renders across the five brief states

    func testCardBadgeHostsAndRendersEveryState() {
        let states: [ReviewGuideCardPresentation] = [
            ReviewGuideCardPresentation.from(lifecycle: "generating", readableVersionId: nil)!,
            ReviewGuideCardPresentation.from(lifecycle: "generating", readableVersionId: "prgv_1")!,
            ReviewGuideCardPresentation.from(lifecycle: "queued", readableVersionId: nil)!,
            ReviewGuideCardPresentation.from(lifecycle: "queued", readableVersionId: "prgv_1")!,
            ReviewGuideCardPresentation.from(lifecycle: "ready", readableVersionId: "prgv_1")!,
            ReviewGuideCardPresentation.from(lifecycle: "failed", readableVersionId: nil)!,
            ReviewGuideCardPresentation.from(lifecycle: "failed", readableVersionId: "prgv_1")!,
        ]
        for presentation in states {
            let view = ReviewGuideCardBadge(presentation: presentation, onOpen: {}, onRetry: {})
            let hosting = NSHostingView(rootView: view)
            hosting.frame = NSRect(x: 0, y: 0, width: 120, height: 30)
            hosting.layoutSubtreeIfNeeded()
            XCTAssertGreaterThan(
                hosting.fittingSize.width, 0,
                "kind \(presentation.kind) must render at least one control"
            )
        }
    }

    // MARK: - Helpers

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    private static func makeTask(id: String, readableVersionId: String?, name: String = "Test work", kind: String = "task") -> WorkTask {
        var task = WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: kind,
            name: name,
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/spinyfin/mono/pull/1",
            deletedAt: nil,
            createdAt: "2026-05-14T00:00:00Z",
            updatedAt: "2026-05-14T00:00:00Z"
        )
        task.reviewGuideLifecycle = readableVersionId == nil ? "queued" : "ready"
        task.reviewGuideReadableVersionId = readableVersionId
        return task
    }
}
