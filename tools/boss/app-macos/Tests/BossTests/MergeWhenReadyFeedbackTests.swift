import XCTest
@testable import Boss

/// Trunk merge-queue design doc, task 8: the `action` value on a
/// `merge_when_ready_accepted` reply (`MergeAction::as_str()` on the
/// engine) drives an inline confirmation banner on the originating card
/// (`ChatViewModel.mergeFeedbackNotice`), with `"trunk_enqueued"` getting
/// its own "Submitted to Trunk merge queue" copy rather than a message
/// shared with the GitHub-native paths.
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

    func testKnownGitHubActionsGetDistinctFeedbackText() {
        let model = makeModel()

        model.applyEventForTest(.mergeWhenReadyAccepted(workItemID: "t", prURL: "u", action: "enqueued"))
        XCTAssertEqual(model.mergeFeedbackNotice?.message, "Submitted to merge queue")

        model.applyEventForTest(.mergeWhenReadyAccepted(workItemID: "t", prURL: "u", action: "auto_merge_enabled"))
        XCTAssertEqual(model.mergeFeedbackNotice?.message, "Merge When Ready armed")

        model.applyEventForTest(.mergeWhenReadyAccepted(workItemID: "t", prURL: "u", action: "merged"))
        XCTAssertEqual(model.mergeFeedbackNotice?.message, "Merged")
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
        model.applyEventForTest(.mergeWhenReadyAccepted(workItemID: "task_2", prURL: "u", action: "merged"))

        XCTAssertEqual(model.mergeFeedbackNotice?.taskID, "task_2")
        XCTAssertEqual(model.mergeFeedbackNotice?.message, "Merged")
    }

    // MARK: - Helpers

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }
}
