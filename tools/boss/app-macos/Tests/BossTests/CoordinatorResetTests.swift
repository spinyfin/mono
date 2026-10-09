import XCTest
@testable import Boss

@MainActor
final class CoordinatorResetTests: XCTestCase {
    func testResetWaitsThenTimeoutOffersForceWithoutGenericError() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var requests: [[String: Any]] = []
        model.outboundRecorder = { requests.append($0) }
        model.attachedCoordinatorSpawnToken = "original"
        model.resetCoordinator()
        XCTAssertTrue(model.coordinatorResetWaiting)
        XCTAssertEqual(model.pendingCoordinatorReset?.token, "original")
        XCTAssertEqual(requests.last?["force_without_handoff"] as? Bool, false)
        model.applyEventForTest(.error(message: "recreate_coordinator: coordinator did not write a handoff within 120s"))
        XCTAssertFalse(model.coordinatorResetWaiting)
        XCTAssertTrue(model.coordinatorResetTimedOut)
        XCTAssertNil(model.workErrorMessage)
        // Force must retain the confirmed token, even if the attached session changed.
        model.attachedCoordinatorSpawnToken = "newer"
        model.forceCoordinatorReset()
        XCTAssertEqual(model.pendingCoordinatorReset?.token, "original")
        XCTAssertTrue(model.coordinatorResetWaiting)
        XCTAssertFalse(model.coordinatorResetTimedOut)
        XCTAssertEqual(requests.last?["expected_spawn_token"] as? String, "original")
        XCTAssertEqual(requests.last?["force_without_handoff"] as? Bool, true)
        XCTAssertEqual(requests.last?["reason"] as? String, "operator_reset")
    }

    func testModelMismatchUsesSameWaitAndRetainsReasonForForce() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.attachedCoordinatorSpawnToken = "original"
        model.attachedCoordinatorModel = "opus"
        model.coordinatorModelConfigured("sonnet")
        model.confirmCoordinatorModelRecreate()
        XCTAssertTrue(model.coordinatorResetWaiting)
        XCTAssertEqual(model.pendingCoordinatorReset?.reason, .modelMismatch)
        model.applyEventForTest(.error(message: "recreate_coordinator: coordinator did not write a handoff within 120s"))
        model.forceCoordinatorReset()
        XCTAssertEqual(model.pendingCoordinatorReset?.reason, .modelMismatch)
    }

    func testOtherResetErrorsAndDisconnectClearWaiting() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.attachedCoordinatorSpawnToken = "original"
        model.resetCoordinator()
        model.applyEventForTest(.error(message: "recreate_coordinator: coordinator changed before confirmation"))
        XCTAssertFalse(model.coordinatorResetWaiting)
        XCTAssertFalse(model.coordinatorResetTimedOut)
        XCTAssertNotNil(model.workErrorMessage)
        model.resetCoordinator()
        model.applyEventForTest(.disconnected)
        XCTAssertFalse(model.coordinatorResetWaiting)
        XCTAssertNil(model.pendingCoordinatorReset)
    }

    func testReplacementClearsWaitingEvenIfViewerCannotAttach() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.attachedCoordinatorSpawnToken = "original"
        model.resetCoordinator()
        model.applyEventForTest(.engineRequest(
            requestId: "attach",
            request: .attachCoordinatorPane(EngineCoordinatorAttachRequest(
                sessionName: "boss-coordinator", spawnToken: "replacement", model: "opus",
                tmuxProgram: "/usr/bin/tmux", tmuxSocketPath: "/tmp/boss-test.sock",
                newerInstalledClaudeVersion: nil
            ))
        ))
        XCTAssertFalse(model.coordinatorResetWaiting)
        XCTAssertNil(model.pendingCoordinatorReset)
    }
}
