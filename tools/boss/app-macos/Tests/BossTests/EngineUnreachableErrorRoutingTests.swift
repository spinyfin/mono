import XCTest
@testable import Boss

/// Covers the routing rule that keeps transport-level socket errors
/// off the modal Work-Error path. The reconnect loop in `EngineClient`
/// emits a fresh `socket waiting:` / `socket failed:` line on every
/// attempt while the engine is unreachable; routing those through
/// `workErrorMessage` made the resulting modal re-pop on every
/// dismissal (#698). Real `work_error` payloads from the engine still
/// drive the modal.
@MainActor
final class EngineUnreachableErrorRoutingTests: XCTestCase {
    func testOverflowPreservesCreateSheetAndReportsUnsavedAction() async {
        let model = makeModel()
        model.applyEventForTest(.retriedActionsStatus(message: "Unconfirmed earlier action"))
        for index in 0..<EngineClient.maxUnsentActions {
            model.engine.sendLine(["type": "create_product", "name": "pending-\(index)"])
        }
        let request = WorkCreateRequest(kind: .product)
        model.pendingWorkCreateRequest = request
        model.submitWorkCreateRequest(request, name: "draft title", description: "draft body")
        XCTAssertEqual(model.pendingWorkCreateRequest?.id, request.id, "the sheet and its text stay open")
        XCTAssertTrue(model.disconnectedActionNotice?.contains("was not saved") == true)
        XCTAssertTrue(model.disconnectedActionNotice?.contains("Unconfirmed earlier action") == true)
        XCTAssertEqual(model.engine.unsentActionCountForTesting, EngineClient.maxUnsentActions)
        model.engine.sendLine(["type": "create_product", "name": "first-overflow"])
        try? await Task.sleep(nanoseconds: 100_000_000)
        XCTAssertTrue(model.disconnectedActionNotice?.contains("was not saved") == true)
        model.dismissDisconnectedActions()
        XCTAssertFalse(model.disconnectedActionNotice?.contains("was not saved") == true, "Discard clears the overflow line")
    }

    func testAutomaticCommentsResolveIsNeverRetainedOrTracked() {
        XCTAssertFalse(EngineClient.isUserMutation(["type": "comments_resolve"]))
        let model = makeModel()
        var resolveCount = 0
        model.engine.outboundRecorder = { payload in
            if payload["type"] as? String == "comments_resolve" { resolveCount += 1 }
        }
        let layer = CommentLayer()
        layer.configure(source: "Document text", baseURL: nil, artifact: .workItem(id: "a"), backend: model.commentBridge)
        layer.reload()
        XCTAssertEqual(resolveCount, 2)
        XCTAssertEqual(model.engine.queuedReadCountForTesting, 2, "only list and banner reads are queued")
        XCTAssertEqual(model.engine.unsentActionCountForTesting, 0)
        XCTAssertFalse(model.hasUnsentActions)
        XCTAssertTrue(model.engine.uncertainActionsForTesting.isEmpty)
    }

    func testRawWireKindIsDescribedInNotSavedNotice() {
        let model = makeModel()
        model.applyEventForTest(.actionNotSaved(requestKind: "create_product"))
        XCTAssertTrue(model.disconnectedActionNotice?.contains("create product was not saved") == true)
    }

    private func parkDrag(_ model: ChatViewModel, taskID: String, retained: Bool) {
        model.pendingDragAdmissionChecks[taskID] = .init(taskID: taskID, column: .doing, group: nil)
        model.pendingMoveOriginByTaskID[taskID] = .backlog
        model.optimisticColumnByTaskID[taskID] = .doing
        if retained { model.engine.sendLine(["type": "evaluate_dispatch_admission", "work_item_id": taskID]) }
    }

    func testUnrelatedErrorDoesNotClearParkedDrag() {
        let model = makeModel()
        parkDrag(model, taskID: "t1", retained: true)
        model.applyEventForTest(.error(message: "unrelated"))
        XCTAssertNotNil(model.pendingDragAdmissionChecks["t1"])
        XCTAssertTrue(model.engine.hasUnsentAction(kind: "evaluate_dispatch_admission"))
    }

    func testUnrelatedWorkErrorPreservesParkedDrag() {
        let model = makeModel()
        parkDrag(model, taskID: "parked", retained: true)
        model.applyEventForTest(.workError(message: "unrelated", requestId: "other"))
        XCTAssertEqual(model.workErrorMessage, "unrelated", "the error must still be surfaced")
        XCTAssertNotNil(model.pendingDragAdmissionChecks["parked"])
        XCTAssertEqual(model.optimisticColumnByTaskID["parked"], .doing)
    }

    func testRetryStatusBouncesOnlyInFlightDrag() {
        let model = makeModel()
        parkDrag(model, taskID: "parked", retained: true)
        parkDrag(model, taskID: "inflight", retained: false)
        model.applyEventForTest(.retriedActionsStatus(message: "Connection lost"))
        XCTAssertNotNil(model.pendingDragAdmissionChecks["parked"])
        XCTAssertEqual(model.optimisticColumnByTaskID["parked"], .doing)
        XCTAssertNil(model.pendingDragAdmissionChecks["inflight"])
        XCTAssertNil(model.optimisticColumnByTaskID["inflight"])
    }

    func testCommentSubmissionAcceptanceClearsUnsavedNotice() {
        let model = makeModel()
        model.showActionNotSaved(requestKind: "comment")
        XCTAssertNotNil(model.unsavedActionNotice)
        XCTAssertTrue(model.commentBridge.canAcceptUserSubmission(describing: "comment"))
        XCTAssertNil(model.unsavedActionNotice)
        XCTAssertNil(model.disconnectedActionNotice)
    }

    func testDisconnectBouncesOnlyInFlightDrag() {
        let model = makeModel()
        parkDrag(model, taskID: "parked", retained: true)
        parkDrag(model, taskID: "inflight", retained: false)
        model.applyEventForTest(.disconnected)
        XCTAssertNotNil(model.pendingDragAdmissionChecks["parked"])
        XCTAssertNil(model.pendingDragAdmissionChecks["inflight"])
        XCTAssertNotNil(model.optimisticColumnByTaskID["parked"])
        XCTAssertNil(model.optimisticColumnByTaskID["inflight"])
    }

    func testDiscardBouncesOnlyParkedDrag() {
        let model = makeModel()
        parkDrag(model, taskID: "parked", retained: true)
        parkDrag(model, taskID: "inflight", retained: false)
        model.dismissDisconnectedActions()
        XCTAssertNil(model.pendingDragAdmissionChecks["parked"])
        XCTAssertNotNil(model.pendingDragAdmissionChecks["inflight"])
    }

    /// `socket waiting:` is what `NWConnection`'s `.waiting` state
    /// emits while the unix socket can't be opened. It must not set
    /// `workErrorMessage` — the disconnected banner is what
    /// communicates this state to the user.
    func testSocketWaitingDoesNotSetWorkErrorMessage() {
        let model = makeModel()
        model.applyEventForTest(.transportError(message: "socket waiting: Connection refused"))
        XCTAssertNil(model.workErrorMessage)
    }

    /// Same routing rule for the terminal `socket failed:` variant.
    func testSocketFailedDoesNotSetWorkErrorMessage() {
        let model = makeModel()
        model.applyEventForTest(.transportError(message: "socket failed: POSIXErrorCode(rawValue: 61)"))
        XCTAssertNil(model.workErrorMessage)
    }

    /// `socket send failed:` and `socket receive failed:` fire when an
    /// existing connection drops mid-stream. They're the same class of
    /// transport error and must follow the same rule.
    func testSocketSendAndReceiveFailedDoNotSetWorkErrorMessage() {
        let model = makeModel()
        model.applyEventForTest(.transportError(message: "socket send failed: connection reset"))
        XCTAssertNil(model.workErrorMessage)

        model.applyEventForTest(.transportError(message: "socket receive failed: connection reset"))
        XCTAssertNil(model.workErrorMessage)
    }

    /// Non-transport `.error` events (engine-reported errors, decode
    /// failures, etc.) still pop the modal — only socket-level signals
    /// are routed away.
    func testNonSocketErrorStillSetsWorkErrorMessage() {
        let model = makeModel()
        model.applyEventForTest(.error(message: "received invalid JSON message from engine"))
        XCTAssertEqual(model.workErrorMessage, "received invalid JSON message from engine")
    }

    /// The engine's first-class `work_error` payload (distinct from the
    /// transport-level `.error` variant) is still the modal path.
    func testWorkErrorEventStillSetsWorkErrorMessage() {
        let model = makeModel()
        model.applyEventForTest(.workError(message: "work item could not be created", requestId: nil))
        XCTAssertEqual(model.workErrorMessage, "work item could not be created")
    }

    func testDisconnectedBoardMoveIsSavedForRetry() {
        let model = makeModel()
        XCTAssertNil(model.engine.sendLine(["type": "move_work_item_on_board", "work_item_id": "t"]))
        model.applyEventForTest(.notConnected(requestKind: "move_work_item_on_board"))
        XCTAssertNotNil(model.disconnectedActionNotice)
        XCTAssertTrue(model.hasUnsentActions)
    }

    /// Even after a transport burst, the message stays clear so the
    /// modal binding never flips to `true`. This is the specific
    /// failure mode from #698: the reconnect loop emits a new
    /// `socket waiting:` line per attempt, so dismissing the modal
    /// only worked until the next retry.
    func testRepeatedSocketWaitingDoesNotResurrectModal() {
        let model = makeModel()
        for _ in 0..<10 {
            model.applyEventForTest(.transportError(message: "socket waiting: Connection refused"))
        }
        XCTAssertNil(model.workErrorMessage)
    }

    /// A send while disconnected is typed `.notConnected`: no modal, and
    /// the connection-lost banner state is left to its own debounce.
    func testDroppedActionStaysVisibleThroughReconnectWithoutSystemMessages() {
        let model = makeModel()
        XCTAssertFalse(model.showSystemMessages)
        // Only an action the engine actually retained earns the Retry notice.
        XCTAssertNil(model.engine.sendLine(["type": "create_chore", "name": "dropped"]))
        model.applyEventForTest(.notConnected(requestKind: "create_chore"))
        XCTAssertNotNil(model.disconnectedActionNotice)
        XCTAssertTrue(model.hasUnsentActions)
        model.applyEventForTest(.connected)
        XCTAssertNotNil(model.disconnectedActionNotice)
        XCTAssertNil(model.workErrorMessage)
        XCTAssertFalse(model.showConnectionLostBanner)
    }

    /// `EngineClient` with no connection: a read is queued (not emitted as
    /// an error); a mutation is failed back as `.notConnected`.
    func testClientQueuesReadsAndReportsNotConnectedForMutations() async {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var events: [EngineEvent] = []
        client.onEvent = { events.append($0) }

        XCTAssertNotNil(client.sendLine(["type": "get_work_tree"], queueIfDisconnected: true))
        XCTAssertNotNil(client.sendLine(["type": "get_work_tree"], queueIfDisconnected: true))
        XCTAssertEqual(client.queuedReadCountForTesting, 1, "identical reads coalesce")
        XCTAssertNotNil(client.sendListEngineAttempts(limit: 20))
        XCTAssertEqual(client.queuedReadCountForTesting, 2, "on-demand history reads must survive backoff")
        XCTAssertNil(client.sendLine(["type": "create_chore"]))

        try? await Task.sleep(nanoseconds: 200_000_000)
        XCTAssertEqual(events.count, 1)
        if case .notConnected(let kind) = events.first { XCTAssertEqual(kind, "create_chore") }
        else { XCTFail("expected .notConnected, got \(events)") }
    }

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    func testRetryConfirmationClearsNoticeAndUncertainDeliverySurvivesReconnect() {
        let model = makeModel()
        model.showDisconnectedActionNotice()
        model.applyEventForTest(.retriedActionsStatus(message: nil))
        XCTAssertNil(model.disconnectedActionNotice)
        let uncertain = "Connection lost before confirmation. Check whether your actions completed before trying again."
        model.applyEventForTest(.retriedActionsStatus(message: uncertain))
        model.applyEventForTest(.connected)
        XCTAssertEqual(model.disconnectedActionNotice, uncertain)
    }

    func testAcknowledgeClearsUncertaintyNoticeOnly() {
        let model = makeModel()
        model.applyEventForTest(.retriedActionsStatus(message: "first"))
        model.engine.sendLine(["type": "create_product", "name": "pending"])
        model.applyEventForTest(.notConnected(requestKind: "create_product"))
        model.acknowledgeUncertainActions()
        XCTAssertNil(model.uncertainActionNotice)
        XCTAssertFalse(model.disconnectedActionNotice?.contains("first") == true)
        XCTAssertTrue(model.engine.hasUnsentActions, "queued actions are not affected by acknowledging")
        // A later drop reports only its own actions.
        model.applyEventForTest(.retriedActionsStatus(message: "second"))
        XCTAssertEqual(model.uncertainActionNotice, "second")
    }

    func testDiscardExplicitlyRemovesRetainedActions() {
        let model = makeModel()
        XCTAssertNil(model.engine.sendLine(["type": "create_product", "name": "discard-me"]))
        XCTAssertTrue(model.engine.hasUnsentActions)
        model.showDisconnectedActionNotice()
        model.dismissDisconnectedActions()
        XCTAssertFalse(model.engine.hasUnsentActions)
        XCTAssertNil(model.disconnectedActionNotice)
    }

    func testUncertaintySurvivesAnotherQueuedActionAcknowledgmentAndDiscard() {
        let model = makeModel()
        let uncertain = "Connection lost before confirmation. Check whether your actions completed before trying again."
        model.applyEventForTest(.retriedActionsStatus(message: uncertain))
        model.engine.sendLine(["type": "create_product", "name": "pending"])
        model.applyEventForTest(.notConnected(requestKind: "create_product"))
        XCTAssertTrue(model.disconnectedActionNotice?.contains(uncertain) == true)
        XCTAssertTrue(model.disconnectedActionNotice?.contains("Your actions are saved") == true)

        model.applyEventForTest(.disconnected)
        model.applyEventForTest(.connected)
        XCTAssertTrue(model.disconnectedActionNotice?.contains(uncertain) == true)
        XCTAssertTrue(model.disconnectedActionNotice?.contains("Retry to send") == true)

        // Acknowledging a different batch must not settle the unknown outcome.
        model.applyEventForTest(.retriedActionsStatus(message: nil))
        XCTAssertTrue(model.disconnectedActionNotice?.contains(uncertain) == true)
        model.dismissDisconnectedActions()
        XCTAssertEqual(model.disconnectedActionNotice, uncertain)
        XCTAssertFalse(model.hasUnsentActions)
    }
}
