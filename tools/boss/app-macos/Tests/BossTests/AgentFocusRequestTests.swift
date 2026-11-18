import XCTest
@testable import Boss

@MainActor
final class AgentFocusRequestTests: XCTestCase {
    func testFocusAgentRunSwitchesFromWorkAfterSuccessfulReveal() {
        let model = makeModel()
        model.navigationMode = .work
        var received: String?
        model.paneFocusHandler = {
            XCTAssertEqual(model.navigationMode, .work)
            received = $0
            return .success
        }
        guard case .success = model.focusAgentRun("run-a") else {
            return XCTFail("expected reveal success")
        }
        XCTAssertEqual(model.navigationMode, .agents)
        XCTAssertEqual(received, "run-a")
        model.paneFocusHandler = nil
    }

    func testFocusFromWorkSwitchesModeAndPassesRun() {
        let model = makeModel()
        model.navigationMode = .work
        var received: String?
        model.paneFocusHandler = { received = $0; return .success }
        model.workErrorMessage = "This agent is no longer available in this window."
        model.focusAgentFromWorkCard("run-a")
        XCTAssertEqual(model.navigationMode, .agents)
        XCTAssertEqual(received, "run-a")
        XCTAssertNil(model.workErrorMessage)
    }

    func testMissingHandlerFailsWithoutSwitchingMode() {
        let model = makeModel()
        model.navigationMode = .work
        guard case .failure(.internalFailure) = model.focusAgentRun("run-a") else {
            return XCTFail("missing handler must fail")
        }
        XCTAssertEqual(model.navigationMode, .work)
        model.focusAgentFromWorkCard("run-a")
        XCTAssertNotNil(model.workErrorMessage)
    }

    func testUnknownRunKeepsModeAndShowsCardError() {
        let model = makeModel()
        model.navigationMode = .work
        model.paneFocusHandler = { _ in .failure(.unknownSlot) }
        model.focusAgentFromWorkCard("gone")
        XCTAssertEqual(model.navigationMode, .work)
        XCTAssertEqual(model.workErrorMessage, "This agent is no longer available in this window.")
    }

    func testRetryWaitsForReadinessAndReportsActualResult() async {
        let coordinator = AgentFocusCoordinator()
        var waits: [Duration] = []
        coordinator.retrySleep = { waits.append($0) }
        let finished = expectation(description: "focused")
        var attempts = 0
        coordinator.report = { runId, result in
            XCTAssertEqual(runId, "run-a")
            guard case .success = result else { return XCTFail("expected focus") }
            finished.fulfill()
        }
        coordinator.request(runId: "run-a") {
            attempts += 1
            return attempts < 3 ? .failure(.internalFailure("no host window")) : .success
        }
        await fulfillment(of: [finished], timeout: 3)
        XCTAssertEqual(attempts, 3)
        XCTAssertEqual(waits, [.milliseconds(50), .milliseconds(50)], "it waits between attempts, and not after the one that succeeds")
    }

    func testNewRequestCancelsOldFocus() async {
        let coordinator = AgentFocusCoordinator()
        let finished = expectation(description: "latest focus")
        coordinator.report = { runId, _ in
            XCTAssertEqual(runId, "new")
            finished.fulfill()
        }
        coordinator.request(runId: "old") { XCTFail("superseded request"); return .success }
        coordinator.request(runId: "new") { .success }
        await fulfillment(of: [finished], timeout: 3)
    }

    func testViewerlessRevealRequestsCardFocusAndReportsMissingWindow() async {
        let workspace = WorkersWorkspaceModel()
        workspace.agentsRevealHandler = { _ in .revealed }
        // The card never mounts, so the request reports only after its whole
        // retry budget. The waits are recorded instead of served: at 20 real
        // 50 ms sleeps the report took a second, and longer than this test's
        // timeout on a host whose timers fire three to four times late.
        var waits: [Duration] = []
        workspace.agentFocus.retrySleep = { waits.append($0) }
        let finished = expectation(description: "card focus attempted")
        workspace.agentFocus.report = { runId, result in
            XCTAssertEqual(runId, "viewerless")
            guard case .failure(.internalFailure(let reason)) = result else {
                return XCTFail("unmounted card must report failure")
            }
            XCTAssertEqual(reason, "agent card has no host window")
            finished.fulfill()
        }
        guard case .success = workspace.revealAgent(runId: "viewerless") else {
            return XCTFail("reveal should succeed")
        }
        await fulfillment(of: [finished], timeout: 3)
        XCTAssertEqual(
            waits.reduce(.zero, +), .seconds(1),
            "an unmounted card is retried for one second before the failure is reported"
        )
    }

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-focus-test-\(UUID().uuidString).sock")
    }
}
