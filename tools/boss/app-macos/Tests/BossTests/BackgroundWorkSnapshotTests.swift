import XCTest
@testable import Boss

/// Ownership of the engine-authored background-work snapshot: it arrives only
/// as `engine.background_work` pushes, the app never polls for it, and the
/// Activity history read is a separate, generation-gated path that does not
/// touch it. The app renders whatever the engine pushed; it adds no
/// source-specific inclusion logic.
@MainActor
final class BackgroundWorkSnapshotTests: XCTestCase {

    // MARK: - Push-fed snapshot

    func testConnectSubscribesToTheTopicAndSendsNoSnapshotRequest() {
        let model = makeModel()
        let recorder = installRecorder(on: model)

        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }

        let subscribed = recorder.value
            .filter { $0["type"] as? String == "subscribe" }
            .flatMap { $0["topics"] as? [String] ?? [] }
        XCTAssertTrue(subscribed.contains("engine.background_work"), "subscribed to \(subscribed)")
        XCTAssertTrue(listEngineAttempts(in: recorder).isEmpty, "connect must not request the snapshot")
        XCTAssertTrue(model.backgroundWork.isEmpty)
    }

    func testNoPeriodicRequestsWhileConnected() async throws {
        let model = makeModel()
        let recorder = installRecorder(on: model)
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        recorder.value.removeAll()

        // The old poll fired every five seconds; nothing may be scheduled at
        // any cadence now, so a short wait is enough to catch a regression
        // that reintroduces a timer with a test-shortened interval.
        try await Task.sleep(for: .milliseconds(400))

        XCTAssertTrue(listEngineAttempts(in: recorder).isEmpty)
    }

    func testInvalidationAndResyncDoNotRequestTheSnapshot() async {
        let model = makeModel()
        let recorder = installRecorder(on: model)
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        recorder.value.removeAll()

        model.applyEventForTest(.workInvalidated(topic: "work.product.prod_1", productId: "prod_1", itemIds: []))
        model.applyEventForTest(.resyncRequired)
        let settled = await model.waitForRefetchBurstsForTest()
        XCTAssertTrue(settled, "the refetch burst never fired")

        XCTAssertTrue(listEngineAttempts(in: recorder).isEmpty)
    }

    func testPushUpdatesTheBadge() {
        let model = makeModel()
        XCTAssertEqual(model.backgroundWorkVisibleCount, 0)

        model.applyEventForTest(.backgroundWorkSnapshot(items: [makeItem(id: "planner:run_1")]))

        XCTAssertEqual(model.backgroundWork.map(\.id), ["planner:run_1"])
        XCTAssertEqual(model.backgroundWorkVisibleCount, 1)
    }

    func testEachPushReplacesTheWholeSnapshotIncludingWithEmpty() {
        let model = makeModel()
        model.applyEventForTest(.backgroundWorkSnapshot(items: [
            makeItem(id: "planner:run_1"),
            makeItem(id: "conflict_remediation:crz_1", kind: "conflict_remediation"),
        ]))
        XCTAssertEqual(model.backgroundWorkVisibleCount, 2)

        model.applyEventForTest(.backgroundWorkSnapshot(items: [makeItem(id: "planner:run_2")]))
        XCTAssertEqual(model.backgroundWork.map(\.id), ["planner:run_2"])

        model.applyEventForTest(.backgroundWorkSnapshot(items: []))
        XCTAssertTrue(model.backgroundWork.isEmpty)
        XCTAssertEqual(model.backgroundWorkVisibleCount, 0)
    }

    func testCountEqualsPushedListIncludingUnknownKinds() {
        let model = makeModel()
        let items = [
            makeItem(id: "planner:run_1", kind: "project_planner"),
            makeItem(id: "conflict_remediation:crz_1", kind: "conflict_remediation"),
            makeItem(id: "future_worker:src_1", kind: "future_worker"),
        ]

        model.applyEventForTest(.backgroundWorkSnapshot(items: items))

        XCTAssertEqual(model.backgroundWork, items)
        XCTAssertEqual(model.backgroundWorkVisibleCount, items.count)
        XCTAssertEqual(
            model.backgroundWork.filter { if case .unknown = $0.kind { return true }; return false }.count,
            1,
            "unknown kinds stay in the snapshot; the app must not filter them out"
        )
    }

    func testDisconnectClearsTheSnapshotAndReconnectWaitsForTheReplay() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        model.applyEventForTest(.backgroundWorkSnapshot(items: [makeItem(id: "planner:run_1")]))

        model.applyEventForTest(.disconnected)
        XCTAssertTrue(model.backgroundWork.isEmpty)
        XCTAssertEqual(model.backgroundWorkVisibleCount, 0)

        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        XCTAssertTrue(model.backgroundWork.isEmpty, "reconnect must not resurrect the previous snapshot")
        model.applyEventForTest(.backgroundWorkSnapshot(items: [makeItem(id: "planner:run_2")]))
        XCTAssertEqual(model.backgroundWork.map(\.id), ["planner:run_2"])
    }

    // MARK: - Activity history reads do not carry the snapshot

    func testHistoryReplyReplacesAttemptsAndLeavesTheSnapshotAlone() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        model.applyEventForTest(.backgroundWorkSnapshot(items: [makeItem(id: "planner:run_1")]))
        model.registerEngineAttemptsRequest(requestId: "history")

        model.applyEventForTest(.engineAttemptsList(attempts: [makeAttempt(id: "crz_1")], requestId: "history"))

        XCTAssertEqual(model.engineAttempts.map(\.id), ["crz_1"])
        XCTAssertEqual(model.backgroundWork.map(\.id), ["planner:run_1"])
    }

    func testHistoryRequestAsksForNoBackgroundWork() throws {
        let model = makeModel()
        let recorder = installRecorder(on: model)
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        model.activityIsVisible = true
        recorder.value.removeAll()

        model.refreshEngineAttempts()

        let requests = listEngineAttempts(in: recorder)
        XCTAssertEqual(requests.count, 1)
        XCTAssertNil(requests[0]["include_background_work"])
    }

    func testQueuedHistoryReplySurvivesRepeatedDisconnectsAndReconnect() throws {
        let model = makeModel()
        model.activityIsVisible = true
        model.refreshEngineAttempts()
        let requestId = try XCTUnwrap(model.engineAttemptsPending.keys.first)
        XCTAssertTrue(model.engine.isReadQueued(requestId: requestId))
        model.applyEventForTest(.disconnected)
        model.applyEventForTest(.disconnected)
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        XCTAssertNotNil(model.engineAttemptsPending[requestId])

        model.applyEventForTest(.engineAttemptsList(attempts: [makeAttempt(id: "history")], requestId: requestId))

        XCTAssertEqual(model.engineAttempts.map(\.id), ["history"])
        XCTAssertTrue(model.engineAttemptsPending.isEmpty)
    }

    func testReplyAfterDisconnectIsIgnored() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        model.registerEngineAttemptsRequest(requestId: "late")
        model.applyEventForTest(.disconnected)

        model.applyEventForTest(.engineAttemptsList(attempts: [makeAttempt(id: "stale")], requestId: "late"))

        XCTAssertTrue(model.engineAttempts.isEmpty)
    }

    func testOutOfOrderHistoryRepliesDoNotApplyAStaleList() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        model.registerEngineAttemptsRequest(requestId: "old")
        model.registerEngineAttemptsRequest(requestId: "new")

        model.applyEventForTest(.engineAttemptsList(attempts: [makeAttempt(id: "newer")], requestId: "new"))
        model.applyEventForTest(.engineAttemptsList(attempts: [makeAttempt(id: "older")], requestId: "old"))

        XCTAssertEqual(model.engineAttempts.map(\.id), ["newer"])
        XCTAssertTrue(model.engineAttemptsPending.isEmpty)
    }

    func testNilRequestIdIsIgnored() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }

        model.applyEventForTest(.engineAttemptsList(attempts: [makeAttempt(id: "cir_1")], requestId: nil))

        XCTAssertTrue(model.engineAttempts.isEmpty)
    }

    func testWorkErrorDropsMatchingPendingRequest() {
        let model = makeModel()
        model.applyEventForTest(.connected)
        defer { model.applyEventForTest(.disconnected) }
        model.registerEngineAttemptsRequest(requestId: "failed")
        model.registerEngineAttemptsRequest(requestId: "other")

        model.applyEventForTest(.workError(message: "db failed", requestId: "failed"))

        XCTAssertNil(model.engineAttemptsPending["failed"])
        XCTAssertNotNil(model.engineAttemptsPending["other"])
    }

    // MARK: - Helpers

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-bgwork-\(UUID().uuidString).sock")
    }

    private func installRecorder(on model: ChatViewModel) -> PayloadRecorder {
        let recorder = PayloadRecorder()
        model.outboundRecorder = { payload in recorder.value.append(payload) }
        return recorder
    }

    private func listEngineAttempts(in recorder: PayloadRecorder) -> [[String: Any]] {
        recorder.value.filter { $0["type"] as? String == "list_engine_attempts" && $0["open_prs_only"] == nil }
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
