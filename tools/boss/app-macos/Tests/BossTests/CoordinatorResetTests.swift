import XCTest
@testable import Boss

@MainActor
final class CoordinatorResetTests: XCTestCase {
    func testMissingHandoffWarningSurvivesReplacement() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.attachedCoordinatorSpawnToken = "original"
        model.resetCoordinator()
        let warning = "Coordinator reset: no fresh handoff was written within 30s. Restarting with potentially missing context."
        model.applyEventForTest(.error(message: warning))
        XCTAssertEqual(model.workErrorMessage, warning)
        model.coordinatorPaneAttached(EngineCoordinatorAttachRequest(
            sessionName: "boss-coordinator", spawnToken: "replacement", model: "opus",
            tmuxProgram: "/usr/bin/tmux", tmuxSocketPath: "/tmp/boss-test.sock",
            newerInstalledClaudeVersion: nil
        ))
        XCTAssertFalse(model.coordinatorResetWaiting)
        XCTAssertEqual(model.workErrorMessage, warning)
    }

    func testResetWaitsThenTimeoutOffersForceWithoutGenericError() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var requests: [[String: Any]] = []
        model.outboundRecorder = { requests.append($0) }
        model.engine.simulateConnectedForTesting = true
        model.attachedCoordinatorSpawnToken = "original"
        model.resetCoordinator()
        XCTAssertTrue(model.coordinatorResetWaiting)
        XCTAssertEqual(model.pendingCoordinatorReset?.token, "original")
        XCTAssertEqual(requests.last?["force_without_handoff"] as? Bool, false)
        model.applyEventForTest(.error(message: "recreate_coordinator: coordinator did not receive the handoff request: delivery timed out. The coordinator session is still running."))
        XCTAssertFalse(model.coordinatorResetWaiting)
        XCTAssertTrue(model.coordinatorResetDeliveryFailed)
        XCTAssertEqual(model.coordinatorResetDeliveryFailureReason, "delivery timed out")
        XCTAssertNil(model.workErrorMessage)
        // Force must retain the confirmed token, even if the attached session changed.
        model.engine.simulateConnectedForTesting = true
        model.attachedCoordinatorSpawnToken = "newer"
        model.forceCoordinatorReset()
        XCTAssertEqual(model.pendingCoordinatorReset?.token, "original")
        XCTAssertTrue(model.coordinatorResetWaiting)
        XCTAssertFalse(model.coordinatorResetDeliveryFailed)
        XCTAssertEqual(requests.last?["expected_spawn_token"] as? String, "original")
        XCTAssertEqual(requests.last?["force_without_handoff"] as? Bool, true)
        XCTAssertEqual(requests.last?["reason"] as? String, "operator_reset")
    }

    func testDeliveryFailureShowsTheEngineReasonAndClearsItWithTheBanner() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.engine.simulateConnectedForTesting = true
        model.attachedCoordinatorSpawnToken = "original"
        model.resetCoordinator()
        let engineMessage = "recreate_coordinator: coordinator did not receive the handoff request: inbox delivery failed: coordinator prompt delivery deadline (15s) expired after the request was written to the coordinator's inbox: the coordinator did not accept it as input before the deadline (it is mid-turn, or its inbound policy is holding cross-session messages). The coordinator session is still running."
        model.applyEventForTest(.error(message: engineMessage))
        XCTAssertTrue(model.coordinatorResetDeliveryFailed)
        XCTAssertEqual(
            model.coordinatorResetDeliveryFailureReason,
            "inbox delivery failed: coordinator prompt delivery deadline (15s) expired after the request was written to the coordinator's inbox: the coordinator did not accept it as input before the deadline (it is mid-turn, or its inbound policy is holding cross-session messages)"
        )
        // Keeping the session dismisses the banner and its reason together.
        model.clearCoordinatorReset()
        XCTAssertFalse(model.coordinatorResetDeliveryFailed)
        XCTAssertNil(model.coordinatorResetDeliveryFailureReason)

        // A credential-step failure is reported as such, not as a transport problem.
        model.resetCoordinator()
        model.applyEventForTest(.error(message: "recreate_coordinator: coordinator did not receive the handoff request: the handoff credential for this session could not be minted (coordinator spawn token does not match the live record). The coordinator session is still running."))
        XCTAssertEqual(
            model.coordinatorResetDeliveryFailureReason,
            "the handoff credential for this session could not be minted (coordinator spawn token does not match the live record)"
        )
        // Forcing the reset clears the reason with the failure state.
        model.forceCoordinatorReset()
        XCTAssertNil(model.coordinatorResetDeliveryFailureReason)
        XCTAssertFalse(model.coordinatorResetDeliveryFailed)
    }

    func testDeliveryFailureWithoutAReasonShowsNoEmptyLine() {
        XCTAssertNil(ChatViewModel.coordinatorHandoffDeliveryFailureReason(
            from: "recreate_coordinator: coordinator did not receive the handoff request. The coordinator session is still running."
        ))
        XCTAssertNil(ChatViewModel.coordinatorHandoffDeliveryFailureReason(from: "recreate_coordinator: something else"))
    }

    func testDisconnectedResetRetainsContextUntilDiscard() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.attachedCoordinatorSpawnToken = "original"
        model.resetCoordinator()
        XCTAssertTrue(model.coordinatorResetWaiting)
        model.applyEventForTest(.notConnected(requestKind: "recreate_coordinator"))
        XCTAssertFalse(model.coordinatorResetWaiting)
        XCTAssertEqual(model.pendingCoordinatorReset?.token, "original")
        XCTAssertTrue(model.engine.hasUnsentAction(kind: "recreate_coordinator"))
        model.applyEventForTest(.disconnected)
        XCTAssertEqual(model.pendingCoordinatorReset?.token, "original")
        model.dismissDisconnectedActions()
        XCTAssertNil(model.pendingCoordinatorReset)
        XCTAssertFalse(model.engine.hasUnsentActions)
    }

    func testModelMismatchUsesSameWaitAndRetainsReasonForForce() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.engine.simulateConnectedForTesting = true
        model.attachedCoordinatorSpawnToken = "original"
        model.attachedCoordinatorModel = "opus"
        model.coordinatorModelConfigured("sonnet")
        model.confirmCoordinatorModelRecreate()
        XCTAssertTrue(model.coordinatorResetWaiting)
        XCTAssertEqual(model.pendingCoordinatorReset?.reason, .modelMismatch)
        model.applyEventForTest(.error(message: "recreate_coordinator: coordinator did not receive the handoff request: delivery timed out. The coordinator session is still running."))
        model.forceCoordinatorReset()
        XCTAssertEqual(model.pendingCoordinatorReset?.reason, .modelMismatch)
    }

    func testOtherResetErrorsAndDisconnectClearWaiting() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.engine.simulateConnectedForTesting = true
        model.attachedCoordinatorSpawnToken = "original"
        model.resetCoordinator()
        model.applyEventForTest(.error(message: "recreate_coordinator: coordinator changed before confirmation"))
        XCTAssertFalse(model.coordinatorResetWaiting)
        XCTAssertFalse(model.coordinatorResetDeliveryFailed)
        XCTAssertNotNil(model.workErrorMessage)
        model.resetCoordinator()
        model.applyEventForTest(.disconnected)
        XCTAssertFalse(model.coordinatorResetWaiting)
        XCTAssertNil(model.pendingCoordinatorReset)
    }

    func testReplacementClearsWaitingEvenIfViewerCannotAttach() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.engine.simulateConnectedForTesting = true
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
