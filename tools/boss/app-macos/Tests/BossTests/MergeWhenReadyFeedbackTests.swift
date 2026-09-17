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

    // MARK: - Helpers

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }
}
