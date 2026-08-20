import XCTest
@testable import Boss

/// Connection-scoped ownership of the engine-authored background-work
/// snapshot: five-second `limit = 0` polling, atomic replace, event-triggered
/// early refresh, and cancel/clear on disconnect. The app renders whatever
/// the engine returned; it does not add source-specific inclusion logic.
@MainActor
final class BackgroundWorkSnapshotTests: XCTestCase {

    func testConnectSendsImmediateBackgroundOnlyPoll() {
        let model = makeModel()
        let recorder = installRecorder(on: model)

        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }

        let polls = backgroundPolls(in: recorder)
        XCTAssertEqual(polls.count, 1, "the first poll must fire immediately on connect")
        XCTAssertEqual(intValue(polls[0], "limit"), 0)
        XCTAssertEqual(polls[0]["include_background_work"] as? Bool, true)
        XCTAssertTrue(model.isBackgroundWorkPolling)
        XCTAssertTrue(model.backgroundWork.isEmpty)
    }

    func testTimerKeepsPollingWhileConnectedAndStopsOnDisconnect() async throws {
        let model = makeModel(pollInterval: 0.05)
        let recorder = installRecorder(on: model)

        model.applyEventForTest(.connected)
        XCTAssertEqual(backgroundPolls(in: recorder).count, 1)

        try await Task.sleep(for: .milliseconds(250))
        let whileConnected = backgroundPolls(in: recorder).count
        XCTAssertGreaterThanOrEqual(whileConnected, 3, "the five-second timer (shortened in tests) must keep polling")
        XCTAssertTrue(model.isBackgroundWorkPolling)

        model.applyEventForTest(.disconnected)
        XCTAssertFalse(model.isBackgroundWorkPolling)
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(
            backgroundPolls(in: recorder).count,
            whileConnected,
            "cancelling the timer must stop further polls"
        )
    }

    func testDisconnectClearsSnapshotAndIgnoresLateTaggedResponse() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        applyList(model, requestId: "first", replacesAttempts: false, backgroundWork: [makeItem(id: "planner:run_1")])
        XCTAssertEqual(model.backgroundWork.map(\.id), ["planner:run_1"])

        model.registerBackgroundWorkRequest(requestId: "late", replacesAttempts: false)
        model.applyEventForTest(.disconnected)

        XCTAssertTrue(model.backgroundWork.isEmpty)
        XCTAssertEqual(model.backgroundWorkVisibleCount, 0)
        XCTAssertFalse(model.isBackgroundWorkPolling)

        model.applyEventForTest(.engineAttemptsList(
            attempts: [],
            backgroundWork: [makeItem(id: "planner:run_stale")],
            requestId: "late"
        ))
        XCTAssertTrue(model.backgroundWork.isEmpty, "a reply after disconnect must not restore chrome")
    }

    func testReconnectRestartsPollingWithEmptySnapshotUntilAFreshReply() {
        let model = makeModel()
        let recorder = installRecorder(on: model)

        model.applyEventForTest(.connected)
        applyList(model, requestId: "first", replacesAttempts: false, backgroundWork: [makeItem(id: "planner:run_1")])
        model.applyEventForTest(.disconnected)
        XCTAssertTrue(model.backgroundWork.isEmpty)

        recorder.value.removeAll()
        model.applyEventForTest(.connected)

        XCTAssertTrue(model.backgroundWork.isEmpty, "reconnect must not resurrect the previous snapshot")
        XCTAssertTrue(model.isBackgroundWorkPolling)
        XCTAssertEqual(backgroundPolls(in: recorder).count, 1)

        applyList(model, requestId: "second", replacesAttempts: false, backgroundWork: [makeItem(id: "planner:run_2")])
        XCTAssertEqual(model.backgroundWork.map(\.id), ["planner:run_2"])
        model.applyEventForTest(.disconnected)
    }

    func testOutOfOrderResponsesDoNotApplyAStaleSnapshot() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        model.registerBackgroundWorkRequest(requestId: "old", replacesAttempts: false)
        model.registerBackgroundWorkRequest(requestId: "new", replacesAttempts: false)

        model.applyEventForTest(.engineAttemptsList(
            attempts: [],
            backgroundWork: [makeItem(id: "newer")],
            requestId: "new"
        ))
        model.applyEventForTest(.engineAttemptsList(
            attempts: [],
            backgroundWork: [makeItem(id: "older")],
            requestId: "old"
        ))

        XCTAssertEqual(model.backgroundWork.map(\.id), ["newer"])
        XCTAssertEqual(model.backgroundWorkVisibleCount, 1)
        model.applyEventForTest(.disconnected)
    }

    func testLateHistoryRefreshStillReplacesAttemptsAfterNewerPoll() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        model.registerBackgroundWorkRequest(requestId: "history", replacesAttempts: true)
        model.registerBackgroundWorkRequest(requestId: "poll", replacesAttempts: false)

        model.applyEventForTest(.engineAttemptsList(
            attempts: [],
            backgroundWork: [makeItem(id: "newer")],
            requestId: "poll"
        ))
        XCTAssertEqual(model.backgroundWork.map(\.id), ["newer"])
        XCTAssertTrue(model.engineAttempts.isEmpty)

        let attempt = makeAttempt(id: "crz_1")
        model.applyEventForTest(.engineAttemptsList(
            attempts: [attempt],
            backgroundWork: [makeItem(id: "older")],
            requestId: "history"
        ))

        XCTAssertEqual(model.engineAttempts.map(\.id), ["crz_1"])
        XCTAssertEqual(
            model.backgroundWork.map(\.id),
            ["newer"],
            "a late history reply must not roll the snapshot back"
        )
        model.applyEventForTest(.disconnected)
    }

    func testNilRequestIdIsIgnored() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        model.applyEventForTest(.engineAttemptsList(
            attempts: [makeAttempt(id: "cir_1")],
            backgroundWork: [makeItem(id: "planner:run_1")],
            requestId: nil
        ))
        XCTAssertTrue(model.backgroundWork.isEmpty)
        XCTAssertTrue(model.engineAttempts.isEmpty)
        model.applyEventForTest(.disconnected)
    }

    func testCountEqualsReturnedListIncludingUnknownKinds() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        let items = [
            makeItem(id: "planner:run_1", kind: "project_planner"),
            makeItem(id: "conflict_remediation:crz_1", kind: "conflict_remediation"),
            makeItem(id: "future_worker:src_1", kind: "future_worker"),
        ]

        applyList(model, requestId: "count", replacesAttempts: false, backgroundWork: items)

        XCTAssertEqual(model.backgroundWork, items)
        XCTAssertEqual(model.backgroundWorkVisibleCount, items.count)
        XCTAssertEqual(model.backgroundWorkVisibleCount, model.backgroundWork.count)
        XCTAssertEqual(
            model.backgroundWork.filter { if case .unknown = $0.kind { return true }; return false }.count,
            1,
            "unknown kinds stay in the snapshot; the app must not filter them out"
        )
    }

    func testCompletionReplacesTheSnapshotWithEmptyWithoutSourceLogic() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        applyList(
            model,
            requestId: "filled",
            replacesAttempts: false,
            backgroundWork: [
                makeItem(id: "planner:run_1", kind: "project_planner"),
                makeItem(id: "conflict_remediation:crz_1", kind: "conflict_remediation"),
            ]
        )
        XCTAssertEqual(model.backgroundWorkVisibleCount, 2)

        applyList(model, requestId: "empty", replacesAttempts: false, backgroundWork: [])

        XCTAssertTrue(model.backgroundWork.isEmpty)
        XCTAssertEqual(model.backgroundWorkVisibleCount, 0)
    }

    func testBackgroundOnlyPollDoesNotReplaceEngineAttempts() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        let attempt = makeAttempt(id: "cir_1")
        applyList(
            model,
            requestId: "history",
            replacesAttempts: true,
            attempts: [attempt],
            backgroundWork: [makeItem(id: "planner:run_1")]
        )
        XCTAssertEqual(model.engineAttempts.map(\.id), ["cir_1"])

        applyList(
            model,
            requestId: "poll",
            replacesAttempts: false,
            backgroundWork: [makeItem(id: "planner:run_2")]
        )

        XCTAssertEqual(model.engineAttempts.map(\.id), ["cir_1"])
        XCTAssertEqual(model.backgroundWork.map(\.id), ["planner:run_2"])
        model.applyEventForTest(.disconnected)
    }

    func testWorkInvalidationTriggersAnEarlyBackgroundPoll() {
        let model = makeModel()
        let recorder = installRecorder(on: model)
        model.applyEventForTest(.connected)
        recorder.value.removeAll()

        model.applyEventForTest(.workInvalidated(topic: "work.product.prod_1", productId: "prod_1", itemIds: []))

        XCTAssertEqual(backgroundPolls(in: recorder).count, 1)
        XCTAssertTrue(model.isBackgroundWorkPolling, "an early refresh must not cancel the timer")
        model.applyEventForTest(.disconnected)
    }

    func testResyncRequiredTriggersAnEarlyBackgroundPoll() {
        let model = makeModel()
        let recorder = installRecorder(on: model)
        model.applyEventForTest(.connected)
        recorder.value.removeAll()

        model.applyEventForTest(.resyncRequired)

        XCTAssertEqual(backgroundPolls(in: recorder).count, 1)
        model.applyEventForTest(.disconnected)
    }

    func testEventTriggeredRefreshCoalescesWhileBackgroundPollIsPending() {
        let model = makeModel()
        let recorder = installRecorder(on: model)
        model.applyEventForTest(.connected)
        recorder.value.removeAll()
        model.registerBackgroundWorkRequest(requestId: "inflight", replacesAttempts: false)

        model.applyEventForTest(.workInvalidated(topic: "work.product.prod_1", productId: "prod_1", itemIds: []))
        model.applyEventForTest(.workInvalidated(topic: "comments.artifact.x", productId: nil, itemIds: []))
        model.applyEventForTest(.resyncRequired)

        XCTAssertEqual(
            backgroundPolls(in: recorder).count,
            0,
            "a background-only request already in flight must absorb the invalidation burst"
        )
        model.applyEventForTest(.disconnected)
    }

    func testSuccessfulApplyPrunesOlderBackgroundOnlyPending() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        model.registerBackgroundWorkRequest(requestId: "old", replacesAttempts: false)
        model.registerBackgroundWorkRequest(requestId: "new", replacesAttempts: false)
        XCTAssertEqual(model.backgroundWorkPending.count, 2)

        model.applyEventForTest(.engineAttemptsList(
            attempts: [],
            backgroundWork: [makeItem(id: "newer")],
            requestId: "new"
        ))

        XCTAssertNil(model.backgroundWorkPending["old"])
        XCTAssertTrue(model.backgroundWorkPending.isEmpty)
        model.applyEventForTest(.disconnected)
    }

    func testSuccessfulApplyKeepsOlderHistoryPendingThatCanStillWinAttempts() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        model.registerBackgroundWorkRequest(requestId: "history", replacesAttempts: true)
        model.registerBackgroundWorkRequest(requestId: "poll", replacesAttempts: false)

        model.applyEventForTest(.engineAttemptsList(
            attempts: [],
            backgroundWork: [makeItem(id: "newer")],
            requestId: "poll"
        ))

        XCTAssertNotNil(
            model.backgroundWorkPending["history"],
            "a history refresh that can still update Activity must not be pruned with the stale snapshot"
        )
        model.applyEventForTest(.disconnected)
    }

    func testWorkErrorDropsMatchingPendingRequest() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        model.registerBackgroundWorkRequest(requestId: "failed", replacesAttempts: false)
        model.registerBackgroundWorkRequest(requestId: "other", replacesAttempts: false)

        model.applyEventForTest(.workError(message: "db failed", requestId: "failed"))

        XCTAssertNil(model.backgroundWorkPending["failed"])
        XCTAssertNotNil(model.backgroundWorkPending["other"])
        model.applyEventForTest(.disconnected)
    }

    func testUnconnectedSendDoesNotRegisterPending() {
        let model = makeModel()
        model.sendBackgroundWorkPoll()
        XCTAssertTrue(model.backgroundWorkPending.isEmpty)
    }

    func testHistoryRefreshReplacesAttemptsAndSnapshotTogether() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        model.registerBackgroundWorkRequest(requestId: "history", replacesAttempts: true)

        let attempt = makeAttempt(id: "crz_1")
        model.applyEventForTest(.engineAttemptsList(
            attempts: [attempt],
            backgroundWork: [makeItem(id: "planner:run_1")],
            requestId: "history"
        ))

        XCTAssertEqual(model.engineAttempts.map(\.id), ["crz_1"])
        XCTAssertEqual(model.backgroundWork.map(\.id), ["planner:run_1"])
        XCTAssertEqual(model.backgroundWorkVisibleCount, model.backgroundWork.count)
        model.applyEventForTest(.disconnected)
    }

    func testStaleGenerationHelperDropsOlderSnapshots() {
        let model = makeModel()
        XCTAssertTrue(model.applyBackgroundWorkSnapshot([makeItem(id: "first")], generation: 2))
        XCTAssertFalse(model.applyBackgroundWorkSnapshot([makeItem(id: "stale")], generation: 1))
        XCTAssertEqual(model.backgroundWork.map(\.id), ["first"])
        XCTAssertTrue(model.applyBackgroundWorkSnapshot([], generation: 3))
        XCTAssertTrue(model.backgroundWork.isEmpty)
    }

    // MARK: - Helpers

    private func applyList(
        _ model: ChatViewModel,
        requestId: String,
        replacesAttempts: Bool,
        attempts: [EngineAttemptListEntry] = [],
        backgroundWork: [BackgroundWorkItem]
    ) {
        model.registerBackgroundWorkRequest(requestId: requestId, replacesAttempts: replacesAttempts)
        model.applyEventForTest(.engineAttemptsList(
            attempts: attempts,
            backgroundWork: backgroundWork,
            requestId: requestId
        ))
    }

    private func makeModel(pollInterval: TimeInterval = ChatViewModel.backgroundWorkPollInterval) -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-bgwork-\(UUID().uuidString).sock")
        model.backgroundWorkPollInterval = pollInterval
        return model
    }

    private func installRecorder(on model: ChatViewModel) -> PayloadRecorder {
        let recorder = PayloadRecorder()
        model.outboundRecorder = { payload in recorder.value.append(payload) }
        return recorder
    }

    private func backgroundPolls(in recorder: PayloadRecorder) -> [[String: Any]] {
        recorder.value.filter { payload in
            payload["type"] as? String == "list_engine_attempts"
                && intValue(payload, "limit") == 0
                && payload["include_background_work"] as? Bool == true
        }
    }

    private func intValue(_ payload: [String: Any], _ key: String) -> Int? {
        if let value = payload[key] as? Int { return value }
        return (payload[key] as? NSNumber)?.intValue
    }

    private func makeItem(id: String, kind: String = "project_planner") -> BackgroundWorkItem {
        BackgroundWorkItem(
            id: id,
            kind: BackgroundWorkKind(rawValue: kind),
            phase: "Working",
            productID: "prod_1",
            sourceID: "src_\(id)",
            title: "Title \(id)",
            projectID: nil,
            startedAt: nil,
            workItemID: nil
        )
    }

    private func makeAttempt(id: String) -> EngineAttemptListEntry {
        EngineAttemptListEntry(
            id: id,
            productID: "prod_1",
            createdAt: "2026-08-19T12:00:00Z",
            extra: [:],
            kind: "ci",
            prURL: "https://github.com/example/repo/pull/42",
            status: "running",
            failureReason: nil,
            finishedAt: nil,
            startedAt: nil,
            workItemID: "task_1"
        )
    }
}

private final class PayloadRecorder: @unchecked Sendable {
    var value: [[String: Any]] = []
}
