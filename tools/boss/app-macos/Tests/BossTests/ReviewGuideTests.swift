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
        XCTAssertNil(artifact, "guide comments are a separate follow-up; no artifact yet")
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

    private static func makeTask(id: String, readableVersionId: String?, name: String = "Test work") -> WorkTask {
        var task = WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "task",
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
