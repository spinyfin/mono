import XCTest
@testable import Boss

@MainActor
final class ReviewTerminalTests: XCTestCase {
    func testRequestIdentityRejectsOldWindowReplies() {
        let model = ReviewTerminalViewModel()
        model.begin(workItemID: "first", taskName: "First", requestID: "old-request")
        XCTAssertTrue(model.windowIsOpen, "Replies before the first frame must be accepted")
        XCTAssertTrue(model.matches(requestID: "old-request", workItemID: "first"))
        model.track(requestID: "old-request")
        model.close()
        XCTAssertFalse(model.windowIsOpen)
        model.begin(workItemID: "second", taskName: "Second")
        model.track(requestID: "new-request")
        XCTAssertFalse(model.matches(requestID: "old-request", workItemID: "first"))
        XCTAssertFalse(model.matches(requestID: "new-request", workItemID: "first"))
        XCTAssertTrue(model.matches(requestID: "new-request", workItemID: "second"))
        model.close()
    }

    func testDisconnectedOpenShowsErrorInsteadOfBlankWindow() {
        let model = ReviewTerminalViewModel()
        model.begin(workItemID: "task", taskName: "Task")
        model.track(requestID: nil)
        guard case .failed(let message) = model.state else { return XCTFail("Expected visible failure") }
        XCTAssertTrue(message.contains("engine"))
    }

    func testReadyRetainsWorkspaceAndRequestForShellDiagnostics() {
        let model = ReviewTerminalViewModel()
        model.begin(workItemID: "task", taskName: "Task")
        model.track(requestID: "request")
        model.noteProgress("Positioning workspace")
        model.ready(ReviewTerminalContent(workItemID: "task", workspacePath: "/tmp/checkout", leaseID: "lease"))
        guard case .ready(let content) = model.state else { return XCTFail("Expected workspace") }
        XCTAssertEqual(content.workspacePath, "/tmp/checkout")
        XCTAssertEqual(content.leaseID, "lease")
        XCTAssertEqual(content.requestID, "request")
        model.close()
    }

    func testSetupFailureRemainsVisible() {
        let model = ReviewTerminalViewModel()
        model.begin(workItemID: "task", taskName: "Task")
        model.track(requestID: "request")
        model.fail("Checkout failed: missing revision")
        guard case .failed(let message) = model.state else { return XCTFail("Expected visible failure") }
        XCTAssertEqual(message, "Checkout failed: missing revision")
        model.close()
    }
}
