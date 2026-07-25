import XCTest
@testable import Boss

/// `deferredScopeActionInFlightIDs` drives the popover row's disabled
/// "acting" state (see `ChatViewModel.deferredScopeActionInFlightIDs`).
/// Mirrors `MergeWhenReadyFeedbackTests`' coverage of the sibling
/// `mergingWhenReadyIDs` in-flight guard.
@MainActor
final class DeferredScopeInFlightTests: XCTestCase {

    func testAcceptWhileDisconnectedSurfacesErrorWithoutMarkingInFlight() {
        let model = makeModel()
        XCTAssertFalse(model.isConnected)

        model.acceptDeferredScopeAttention(id: "attn_1")

        XCTAssertEqual(model.workErrorMessage, "Not connected to the engine — reconnect and try again.")
        XCTAssertFalse(model.deferredScopeActionInFlightIDs.contains("attn_1"))
    }

    func testCreateTaskWhileAlreadyInFlightIsANoOp() {
        let model = makeModel()
        model.deferredScopeActionInFlightIDs.insert("attn_1")
        model.workErrorMessage = nil

        model.createTaskFromDeferredScopeAttention(attentionID: "attn_1")

        XCTAssertNil(model.workErrorMessage, "a duplicate tap while in flight must not re-guard or surface an error")
        XCTAssertTrue(model.deferredScopeActionInFlightIDs.contains("attn_1"))
    }

    func testAttentionItemUpdatedPushClearsInFlightID() {
        let model = makeModel()
        model.deferredScopeActionInFlightIDs.insert("attn_1")

        model.applyEventForTest(.attentionItemUpdated(item: makeDeferredScopeItem(id: "attn_1")))

        XCTAssertFalse(model.deferredScopeActionInFlightIDs.contains("attn_1"))
    }

    func testWorkErrorClearsAllInFlightIDs() {
        let model = makeModel()
        model.deferredScopeActionInFlightIDs = ["attn_1", "attn_2"]

        model.applyEventForTest(.workError(message: "boom"))

        XCTAssertTrue(model.deferredScopeActionInFlightIDs.isEmpty)
    }

    func testDisconnectClearsAllInFlightIDs() {
        let model = makeModel()
        model.deferredScopeActionInFlightIDs = ["attn_1", "attn_2"]

        model.applyEventForTest(.disconnected)

        XCTAssertTrue(model.deferredScopeActionInFlightIDs.isEmpty, "a request in flight when the link drops can never complete")
    }

    // MARK: - Helpers

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    private func makeDeferredScopeItem(id: String) -> WorkAttentionItem {
        WorkAttentionItem(
            id: id,
            executionID: "exec_1",
            workItemID: nil,
            kind: "deferred_scope",
            status: "open",
            title: "Some deferred scope",
            bodyMarkdown: "",
            createdAt: "2026-01-01T00:00:00Z",
            resolvedAt: nil,
            convertedTaskID: nil
        )
    }
}
