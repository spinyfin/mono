import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// Trunk merge-queue design doc, task 8: the `action` value on a
/// `merge_when_ready_accepted` reply (`MergeAction::as_str()` on the
/// engine) drives an inline confirmation banner on the originating card
/// (`ChatViewModel.mergeFeedbackNotice`), with `"trunk_enqueued"` getting
/// its own "Submitted to Trunk merge queue" copy and
/// `"trunk_already_enqueued"` getting "Already in Trunk merge queue" so a
/// duplicate click is not presented as a fresh submission.
@MainActor
final class MergeWhenReadyFeedbackTests: XCTestCase {

    func testTrunkEnqueuedSetsDistinctFeedbackText() {
        let model = makeModel()
        model.mergingWhenReadyIDs.insert("task_1")

        model.applyEventForTest(.mergeWhenReadyAccepted(
            workItemID: "task_1",
            prURL: "https://github.com/x/y/pull/1",
            action: "trunk_enqueued"
        ))

        XCTAssertEqual(model.mergeFeedbackNotice?.taskID, "task_1")
        XCTAssertEqual(model.mergeFeedbackNotice?.message, "Submitted to Trunk merge queue")
        XCTAssertFalse(model.mergingWhenReadyIDs.contains("task_1"), "in-flight guard must clear on any accepted action")
    }

    func testTrunkAlreadyEnqueuedSetsDistinctFeedbackText() {
        let model = makeModel()
        model.mergingWhenReadyIDs.insert("task_1")

        model.applyEventForTest(.mergeWhenReadyAccepted(
            workItemID: "task_1",
            prURL: "https://github.com/x/y/pull/1",
            action: "trunk_already_enqueued"
        ))

        XCTAssertEqual(model.mergeFeedbackNotice?.message, "Already in Trunk merge queue")
        XCTAssertFalse(model.mergingWhenReadyIDs.contains("task_1"), "in-flight guard must clear on any accepted action")
    }

    func testGitHubMergeRequestGetsFeedbackText() {
        let model = makeModel()

        model.applyEventForTest(.mergeWhenReadyAccepted(workItemID: "t", prURL: "u", action: "merge_requested"))
        XCTAssertEqual(model.mergeFeedbackNotice?.message, "Merge requested")
    }

    func testUnrecognisedActionFallsBackRatherThanCrashing() {
        let model = makeModel()
        model.applyEventForTest(.mergeWhenReadyAccepted(workItemID: "t", prURL: "u", action: "some_future_action"))
        XCTAssertEqual(model.mergeFeedbackNotice?.message, "Merge requested")
    }

    func testClearMergeFeedbackDismissesNotice() {
        let model = makeModel()
        model.applyEventForTest(.mergeWhenReadyAccepted(workItemID: "task_1", prURL: "u", action: "trunk_enqueued"))
        XCTAssertNotNil(model.mergeFeedbackNotice)

        model.clearMergeFeedback()
        XCTAssertNil(model.mergeFeedbackNotice)
    }

    func testSecondAcceptedActionReplacesThePreviousNotice() {
        let model = makeModel()
        model.applyEventForTest(.mergeWhenReadyAccepted(workItemID: "task_1", prURL: "u", action: "trunk_enqueued"))
        model.applyEventForTest(.mergeWhenReadyAccepted(workItemID: "task_2", prURL: "u", action: "merge_requested"))

        XCTAssertEqual(model.mergeFeedbackNotice?.taskID, "task_2")
        XCTAssertEqual(model.mergeFeedbackNotice?.message, "Merge requested")
    }

    /// A failed merge, then a retry that is accepted, must leave the error
    /// map empty — otherwise dismissing the success banner resurrects the
    /// stale failure for the rest of the session.
    func testFailRetryAcceptClearsMergeErrorNotice() {
        let model = makeModel()
        var task = WorkTask(
            id: "task_1",
            productID: "prod_test",
            projectID: nil,
            kind: "task",
            name: "Merge me",
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/x/y/pull/1",
            deletedAt: nil,
            createdAt: "2026-05-14T00:00:00Z",
            updatedAt: "2026-05-14T00:00:00Z"
        )
        task.mergeQueueState = nil
        model.taskIndexByID = [task.id: task]

        model.mergeWhenReady(for: task)
        XCTAssertTrue(model.mergingWhenReadyIDs.contains(task.id))

        model.applyEventForTest(.workError(message: "merge failed", requestId: nil))
        XCTAssertEqual(model.mergeErrorNoticesByTaskID[task.id], "merge failed")
        XCTAssertTrue(model.mergingWhenReadyIDs.isEmpty)

        model.mergeWhenReady(for: task)
        XCTAssertNil(
            model.mergeErrorNoticesByTaskID[task.id],
            "a fresh attempt must clear the stale error immediately"
        )

        model.applyEventForTest(.mergeWhenReadyAccepted(
            workItemID: task.id,
            prURL: "https://github.com/x/y/pull/1",
            action: "trunk_enqueued"
        ))
        XCTAssertTrue(model.mergeErrorNoticesByTaskID.isEmpty)
        XCTAssertEqual(model.mergeFeedbackNotice?.taskID, task.id)

        model.clearMergeFeedback()
        XCTAssertTrue(
            model.mergeErrorNoticesByTaskID.isEmpty,
            "dismissing the success banner must not resurrect the failure"
        )
    }

    // MARK: - Shared control (card + review-guide viewer)

    /// The extracted `MergeWhenReadyControl` (design: "Extract the current
    /// card merge control and confirmation into one reusable
    /// presentation/action component, used by both card and viewer") hosts
    /// and lays out on its own, independent of a `WorkCardSnapshot`. The
    /// card and the review-guide viewer header both mount it with only an
    /// `onConfirm` closure that calls `mergeWhenReady(for:)` — this test
    /// pins that the shared view itself renders without needing snapshot
    /// or task context.
    @MainActor
    func testMergeWhenReadyControlHostsAndRenders() {
        let view = MergeWhenReadyControl(onConfirm: {})
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 60, height: 30)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.width, 0)
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    func testConfirmationAlertListsHumanStatusWithoutIdPrefix() {
        let confirmation = ChatViewModel.MergeRevisionConfirmation(
            workItemID: "root",
            revisions: [
                OpenMergeRevision(id: "r1", label: "rev-a", status: "active"),
                OpenMergeRevision(id: "r2", label: "rev-b", status: "todo"),
                OpenMergeRevision(id: "r3", label: "rev-c", status: "blocked"),
            ],
            origin: .board
        )
        XCTAssertEqual(
            confirmation.alertMessage,
            "rev-a — running\nrev-b — queued\nrev-c — blocked\n\nThese revisions may still change this PR. Merge anyway?"
        )
        XCTAssertFalse(confirmation.alertMessage.contains("ID "))
        XCTAssertFalse(confirmation.alertMessage.contains("active"))
        XCTAssertFalse(confirmation.alertMessage.contains("todo"))
    }

    func testOpenRevisionConfirmationRequiresExplicitConsent() {
        let model = makeModel()
        let revisions = [OpenMergeRevision(id: "revision", label: "ID-test", status: "blocked")]
        var requests: [[String: Any]] = []
        model.engine.outboundRecorder = { requests.append($0) }

        model.mergingWhenReadyIDs.insert("root")
        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "root", revisions: revisions))
        XCTAssertTrue(requests.isEmpty)
        XCTAssertEqual(model.pendingMergeRevisionConfirmation?.workItemID, "root")
        XCTAssertEqual(model.pendingMergeRevisionConfirmation?.revisions, revisions)
        model.cancelMergeRevisionConfirmation(workItemID: "root")
        XCTAssertTrue(requests.isEmpty)
        XCTAssertNil(model.pendingMergeRevisionConfirmation)
        XCTAssertFalse(model.mergingWhenReadyIDs.contains("root"))

        model.mergingWhenReadyIDs.insert("root")
        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "root", revisions: revisions))
        model.confirmMergeRevision(workItemID: "root")
        XCTAssertNil(model.pendingMergeRevisionConfirmation)
        XCTAssertEqual(requests.last?["type"] as? String, "merge_when_ready")
        XCTAssertEqual(requests.last?["confirmed_revisions"] as? [[String: String]], revisions.map(\.wirePayload))
        XCTAssertTrue(model.mergingWhenReadyIDs.contains("root"))
    }

    func testSecondConfirmationIsQueuedWhileFirstIsPending() {
        let model = makeModel()
        let first = [OpenMergeRevision(id: "r1", label: "one", status: "open")]
        let second = [OpenMergeRevision(id: "r2", label: "two", status: "blocked")]
        model.mergingWhenReadyIDs = ["task_a", "task_b"]

        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "task_a", revisions: first))
        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "task_b", revisions: second))

        XCTAssertEqual(model.pendingMergeRevisionConfirmation?.workItemID, "task_a")
        XCTAssertEqual(model.pendingMergeRevisionConfirmation?.revisions, first)
        XCTAssertEqual(model.queuedMergeRevisionConfirmations.map(\.workItemID), ["task_b"])
        XCTAssertEqual(model.queuedMergeRevisionConfirmations.first?.revisions, second)
    }

    func testConfirmingFirstPromotesQueuedSecondAndSendsOnlyFirstRevisions() async {
        let model = makeModel()
        let first = [OpenMergeRevision(id: "r1", label: "one", status: "open")]
        let second = [OpenMergeRevision(id: "r2", label: "two", status: "blocked")]
        var requests: [[String: Any]] = []
        model.engine.outboundRecorder = { requests.append($0) }
        model.mergingWhenReadyIDs = ["task_a", "task_b"]

        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "task_a", revisions: first))
        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "task_b", revisions: second))
        model.confirmMergeRevision(workItemID: "task_a")

        XCTAssertEqual(requests.count, 1)
        XCTAssertEqual(requests[0]["type"] as? String, "merge_when_ready")
        XCTAssertEqual(requests[0]["work_item_id"] as? String, "task_a")
        XCTAssertEqual(requests[0]["confirmed_revisions"] as? [[String: String]], first.map(\.wirePayload))

        await waitForMainQueueTurn()

        XCTAssertEqual(model.pendingMergeRevisionConfirmation?.workItemID, "task_b")
        XCTAssertEqual(model.pendingMergeRevisionConfirmation?.revisions, second)
        XCTAssertTrue(model.queuedMergeRevisionConfirmations.isEmpty)
        XCTAssertEqual(requests.count, 1, "promoting the queued confirmation must not send merge_when_ready")
    }

    func testCancellingFirstPromotesQueuedSecondWithoutSending() async {
        let model = makeModel()
        let first = [OpenMergeRevision(id: "r1", label: "one", status: "open")]
        let second = [OpenMergeRevision(id: "r2", label: "two", status: "blocked")]
        var requests: [[String: Any]] = []
        model.engine.outboundRecorder = { requests.append($0) }
        model.mergingWhenReadyIDs = ["task_a", "task_b"]

        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "task_a", revisions: first))
        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "task_b", revisions: second))
        model.cancelMergeRevisionConfirmation(workItemID: "task_a")
        XCTAssertTrue(requests.isEmpty)
        XCTAssertFalse(model.mergingWhenReadyIDs.contains("task_a"))

        await waitForMainQueueTurn()

        XCTAssertEqual(model.pendingMergeRevisionConfirmation?.workItemID, "task_b")
        XCTAssertTrue(model.mergingWhenReadyIDs.contains("task_b"))
        XCTAssertTrue(requests.isEmpty)
    }

    func testDeferredPromotionDoesNotOverwriteNewerPendingConfirmation() async {
        let model = makeModel()
        let first = [OpenMergeRevision(id: "r1", label: "one", status: "open")]
        let second = [OpenMergeRevision(id: "r2", label: "two", status: "blocked")]
        let third = [OpenMergeRevision(id: "r3", label: "three", status: "open")]
        model.mergingWhenReadyIDs = ["task_a", "task_b", "task_c"]

        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "task_a", revisions: first))
        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "task_b", revisions: second))
        model.confirmMergeRevision(workItemID: "task_a")
        XCTAssertNil(model.pendingMergeRevisionConfirmation)

        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "task_c", revisions: third))
        XCTAssertEqual(model.pendingMergeRevisionConfirmation?.workItemID, "task_c")

        await waitForMainQueueTurn()

        XCTAssertEqual(
            model.pendingMergeRevisionConfirmation?.workItemID, "task_c",
            "a confirmation that arrived into the empty slot must not be overwritten by deferred promotion"
        )
        XCTAssertEqual(model.queuedMergeRevisionConfirmations.map(\.workItemID), ["task_b"])
        XCTAssertTrue(model.mergingWhenReadyIDs.contains("task_c"))
        XCTAssertTrue(model.mergingWhenReadyIDs.contains("task_b"))
    }

    func testPromotionDropsQueuedConfirmationAfterInFlightGuardCleared() async {
        let model = makeModel()
        let first = [OpenMergeRevision(id: "r1", label: "one", status: "open")]
        let second = [OpenMergeRevision(id: "r2", label: "two", status: "blocked")]
        model.mergingWhenReadyIDs = ["task_a", "task_b"]

        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "task_a", revisions: first))
        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "task_b", revisions: second))
        model.applyEventForTest(.workError(message: "merge failed", requestId: nil))
        XCTAssertTrue(model.mergingWhenReadyIDs.isEmpty)
        XCTAssertEqual(model.pendingMergeRevisionConfirmation?.workItemID, "task_a")

        model.cancelMergeRevisionConfirmation(workItemID: "task_a")
        await waitForMainQueueTurn()

        XCTAssertNil(model.pendingMergeRevisionConfirmation)
        XCTAssertTrue(model.queuedMergeRevisionConfirmations.isEmpty)
    }

    func testMismatchedConfirmAndCancelAreIgnored() {
        let model = makeModel()
        let revisions = [OpenMergeRevision(id: "r1", label: "one", status: "open")]
        var requests: [[String: Any]] = []
        model.engine.outboundRecorder = { requests.append($0) }
        model.mergingWhenReadyIDs.insert("task_a")
        model.applyEventForTest(.mergeConfirmationRequired(workItemID: "task_a", revisions: revisions))

        model.confirmMergeRevision(workItemID: "task_other")
        model.cancelMergeRevisionConfirmation(workItemID: "task_other")

        XCTAssertEqual(model.pendingMergeRevisionConfirmation?.workItemID, "task_a")
        XCTAssertTrue(model.mergingWhenReadyIDs.contains("task_a"))
        XCTAssertTrue(requests.isEmpty)
    }

    func testViewerOriginPresentsOnViewerWhenOpenAndFallsBackToBoardWhenClosed() {
        let model = makeModel()
        let task = makeMergeTask(id: "task_v")
        model.taskIndexByID = [task.id: task]
        model.isReviewGuideViewerWindowOpen = true
        model.mergeWhenReady(for: task, origin: .reviewGuideViewer)
        model.applyEventForTest(.mergeConfirmationRequired(
            workItemID: task.id,
            revisions: [OpenMergeRevision(id: "r", label: "L", status: "open")]
        ))

        XCTAssertEqual(model.pendingMergeRevisionConfirmation?.origin, .reviewGuideViewer)
        XCTAssertTrue(model.shouldPresentMergeRevisionConfirmation(on: .reviewGuideViewer))
        XCTAssertFalse(model.shouldPresentMergeRevisionConfirmation(on: .board))
        XCTAssertEqual(model.mergeRevisionConfirmation(for: .reviewGuideViewer)?.workItemID, task.id)
        XCTAssertNil(model.mergeRevisionConfirmation(for: .board))

        model.isReviewGuideViewerWindowOpen = false
        XCTAssertFalse(model.shouldPresentMergeRevisionConfirmation(on: .reviewGuideViewer))
        XCTAssertTrue(model.shouldPresentMergeRevisionConfirmation(on: .board))
        XCTAssertEqual(model.mergeRevisionConfirmation(for: .board)?.workItemID, task.id)
        XCTAssertNil(model.mergeRevisionConfirmation(for: .reviewGuideViewer))
    }

    func testBoardOriginDoesNotPresentOnViewerEvenWhenViewerIsOpen() {
        let model = makeModel()
        let task = makeMergeTask(id: "task_board")
        model.taskIndexByID = [task.id: task]
        model.isReviewGuideViewerWindowOpen = true
        model.mergeWhenReady(for: task, origin: .board)
        model.applyEventForTest(.mergeConfirmationRequired(
            workItemID: task.id,
            revisions: [OpenMergeRevision(id: "r", label: "L", status: "open")]
        ))

        XCTAssertEqual(model.pendingMergeRevisionConfirmation?.origin, .board)
        XCTAssertTrue(model.shouldPresentMergeRevisionConfirmation(on: .board))
        XCTAssertFalse(model.shouldPresentMergeRevisionConfirmation(on: .reviewGuideViewer))
    }

    func testConfirmationAlertModifierHostsOnEachSurface() {
        let model = makeModel()
        model.isReviewGuideViewerWindowOpen = true
        model.mergingWhenReadyIDs.insert("root")
        model.mergeRevisionConfirmationOrigins["root"] = .reviewGuideViewer
        model.handleMergeConfirmation(
            workItemID: "root",
            revisions: [OpenMergeRevision(id: "r", label: "L", status: "open")]
        )

        let board = NSHostingView(
            rootView: Text("board").mergeRevisionConfirmationAlert(model: model, surface: .board)
        )
        let viewer = NSHostingView(
            rootView: Text("viewer").mergeRevisionConfirmationAlert(model: model, surface: .reviewGuideViewer)
        )
        board.frame = NSRect(x: 0, y: 0, width: 200, height: 80)
        viewer.frame = NSRect(x: 0, y: 0, width: 200, height: 80)
        board.layoutSubtreeIfNeeded()
        viewer.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(board.fittingSize.width, 0)
        XCTAssertGreaterThan(viewer.fittingSize.width, 0)
        XCTAssertTrue(model.shouldPresentMergeRevisionConfirmation(on: .reviewGuideViewer))
        XCTAssertFalse(model.shouldPresentMergeRevisionConfirmation(on: .board))
    }

    func testNilFindingsReplyDoesNotClearADifferentSeries() {
        let model = makeModel()
        let findingsA = ReviewGuideFindings(
            statusText: "series A status",
            addendumMarkdown: "## A"
        )
        model.applyEventForTest(.reviewGuideFindings(rootTaskId: "root", seriesId: "series_a", findings: findingsA))
        XCTAssertEqual(model.reviewGuideFindingsBySeriesID["series_a"], findingsA)

        model.applyEventForTest(.reviewGuideFindings(rootTaskId: "root", seriesId: "series_b", findings: nil))
        XCTAssertEqual(
            model.reviewGuideFindingsBySeriesID["series_a"], findingsA,
            "a nil reply for series B must not clear cached series A"
        )
        XCTAssertNil(model.reviewGuideFindingsBySeriesID["series_b"])

        model.applyEventForTest(.reviewGuideFindings(rootTaskId: "root", seriesId: "series_a", findings: nil))
        XCTAssertNil(model.reviewGuideFindingsBySeriesID["series_a"])
    }

    func testNonNilFindingsRepliesAreKeyedBySeries() {
        let model = makeModel()
        let findingsA = ReviewGuideFindings(statusText: "A", addendumMarkdown: "## A")
        let findingsB = ReviewGuideFindings(statusText: "B", addendumMarkdown: "## B")
        model.applyEventForTest(.reviewGuideFindings(rootTaskId: "root", seriesId: "series_a", findings: findingsA))
        model.applyEventForTest(.reviewGuideFindings(rootTaskId: "root", seriesId: "series_b", findings: findingsB))

        XCTAssertEqual(model.reviewGuideFindingsBySeriesID["series_a"], findingsA)
        XCTAssertEqual(model.reviewGuideFindingsBySeriesID["series_b"], findingsB)

        let findingsA2 = ReviewGuideFindings(statusText: "A2", addendumMarkdown: "## A2")
        model.applyEventForTest(.reviewGuideFindings(rootTaskId: "root", seriesId: "series_a", findings: findingsA2))
        XCTAssertEqual(model.reviewGuideFindingsBySeriesID["series_a"], findingsA2)
        XCTAssertEqual(
            model.reviewGuideFindingsBySeriesID["series_b"], findingsB,
            "a non-nil reply for series A must not replace series B"
        )
    }

    func testFindingsProjectionPreservesEngineTextAndOptionalCompatibility() {
        XCTAssertNil(ReviewGuideFindings.parse(nil))
        let state = ReviewGuideFindings.parse([
            "status_text": "AI review found 1 issue; fixes complete",
            "addendum_markdown": "## Addendum\n\n- [high] Check bounds — ID example (done)"
        ])
        XCTAssertEqual(state?.statusText, "AI review found 1 issue; fixes complete")
        XCTAssertTrue(state?.addendumMarkdown.contains("Check bounds") == true)
    }

    // MARK: - Helpers

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    private func makeMergeTask(id: String) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "task",
            name: "Merge me",
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/x/y/pull/1",
            deletedAt: nil,
            createdAt: "2026-05-14T00:00:00Z",
            updatedAt: "2026-05-14T00:00:00Z"
        )
    }

    private func waitForMainQueueTurn() async {
        await withCheckedContinuation { continuation in
            DispatchQueue.main.async {
                continuation.resume()
            }
        }
    }
}
