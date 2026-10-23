import XCTest
@testable import Boss

/// A burst of `work_invalidated` pushes must cost one set of requests per
/// product, not one set per push: the tree, the deferred-scope list, the
/// background-work poll, and `list_executions` for the invalidated items that
/// something is showing. Planner state rides in the tree, so a burst sends no
/// `list_planner_runs` however many projects the product has. See
/// `ChatViewModel+InvalidationRefetch.swift`.
///
/// These tests run on real timers with a short window and wait for the
/// first request to appear rather than for a fixed time, so a slow machine
/// delays them instead of failing them.
@MainActor
final class InvalidationRefetchCoalescingTests: XCTestCase {

    // MARK: - Coalescing

    func testBurstOfInvalidationsSendsOneSetOfRequests() async {
        let (model, recorder) = makeModel()

        for index in 0..<10 {
            model.applyEventForTest(invalidation(itemIDs: ["task_\(index)"]))
        }
        XCTAssertTrue(
            recorder.sent.isEmpty,
            "nothing is sent while the window is open; got \(recorder.counts)"
        )

        await waitForBurst(model, recorder)

        XCTAssertEqual(
            recorder.counts,
            [
                "get_work_tree": 1,
                "list_deferred_scope_attentions": 1,
                "list_engine_attempts": 1,
            ],
            "ten invalidations must cost exactly one set, and none of it is per project"
        )
    }

    func testTwoProductsProduceTwoBursts() async {
        let (model, recorder) = makeModel()

        for _ in 0..<3 {
            model.applyEventForTest(invalidation(itemIDs: ["task_a"]))
            model.scheduleWorkTreeRefetch(productID: "prod_b", flow: .itemRefetch)
        }

        await waitForBurst(model, recorder)

        XCTAssertEqual(
            recorder.treeFetches.sorted(),
            ["prod_a", "prod_b"],
            "each product gets exactly one tree fetch, not one per trigger"
        )
        XCTAssertEqual(
            recorder.counts["list_deferred_scope_attentions"], 1,
            "only the product on screen has its side list refetched"
        )
        XCTAssertNil(recorder.counts["list_planner_runs"])
    }

    func testTreeOnlyTriggerJoinsThePendingInvalidationBurst() async {
        let (model, recorder) = makeModel()

        model.scheduleWorkTreeRefetch(productID: "prod_a", flow: .itemRefetch)
        model.applyEventForTest(invalidation(itemIDs: ["task_1"]))
        model.scheduleWorkTreeRefetch(productID: "prod_a", flow: .itemRefetch)

        await waitForBurst(model, recorder)

        XCTAssertEqual(recorder.treeFetches, ["prod_a"], "a reorder and an invalidation share one tree fetch")
        XCTAssertEqual(
            recorder.counts["list_deferred_scope_attentions"], 1,
            "the invalidation's side-fetches are not lost to the tree-only trigger"
        )
    }

    func testTreeOnlyTriggerDoesNotSendSideFetches() async {
        let (model, recorder) = makeModel()

        model.scheduleWorkTreeRefetch(productID: "prod_a", flow: .itemRefetch)
        model.scheduleWorkTreeRefetch(productID: "prod_a", flow: .itemRefetch)

        await waitForBurst(model, recorder)

        XCTAssertEqual(recorder.counts, ["get_work_tree": 1])
    }

    // MARK: - Trailing edge

    /// Each trigger restarts the product's single timer and adds to what the
    /// burst will send. Written against a window that cannot elapse during
    /// the test, then shortened for the last trigger, so no assertion
    /// depends on how long the host takes between two statements.
    func testEachTriggerRestartsTheTimerAndFiresOnceWithTheUnionOfItems() async {
        let (model, recorder) = makeModel(policy: .init(window: .seconds(60), maxWait: .seconds(120)))
        openConsumers(model, ["task_1", "task_2", "task_3", "task_4"])
        recorder.reset()

        model.applyEventForTest(invalidation(itemIDs: ["task_1"]))
        let first = model.pendingWorkTreeRefetchTasks["prod_a"]
        model.applyEventForTest(invalidation(itemIDs: ["task_2"]))
        let second = model.pendingWorkTreeRefetchTasks["prod_a"]

        XCTAssertEqual(first?.isCancelled, true, "a later trigger replaces the earlier timer")
        XCTAssertEqual(second?.isCancelled, false)
        XCTAssertEqual(model.pendingRefetchBursts["prod_a"]?.itemIDs, ["task_1", "task_2"])
        XCTAssertTrue(recorder.sent.isEmpty, "nothing is sent before the window closes")

        model.refetchCoalescingPolicy = .init(window: .milliseconds(40), maxWait: .seconds(2))
        model.applyEventForTest(invalidation(itemIDs: ["task_3"]))
        await waitForBurst(model, recorder)

        XCTAssertEqual(recorder.counts["get_work_tree"], 1)
        XCTAssertEqual(
            recorder.executionFetches, ["task_1", "task_2", "task_3"],
            "the burst refetches the union of what the triggers named, and no other consumer"
        )
        XCTAssertTrue(model.pendingRefetchBursts.isEmpty, "a fired burst leaves nothing pending")
    }

    func testTrailingEdgeFiresAfterEachBurst() async {
        let (model, recorder) = makeModel()

        model.applyEventForTest(invalidation(itemIDs: ["task_1"]))
        model.applyEventForTest(invalidation(itemIDs: ["task_2"]))
        await waitForBurst(model, recorder)
        XCTAssertEqual(recorder.counts["get_work_tree"], 1)

        recorder.reset()
        model.backgroundWorkPending.removeAll()
        model.applyEventForTest(invalidation(itemIDs: ["task_3"]))
        await waitForBurst(model, recorder)

        XCTAssertEqual(
            recorder.counts,
            [
                "get_work_tree": 1,
                "list_deferred_scope_attentions": 1,
                "list_engine_attempts": 1,
            ],
            "a trigger after a fired burst starts a new burst, whose trailing edge fires"
        )
    }

    func testSustainedStreamStillFiresBeforeItEndsAndOnceAfter() async {
        let (model, recorder) = makeModel(policy: .init(window: .milliseconds(100), maxWait: .milliseconds(300)))

        // Triggers 50 ms apart never leave the 100 ms window quiet.
        var treesWhileStreaming = 0
        for _ in 0..<14 {
            model.applyEventForTest(invalidation(itemIDs: ["task_1"]))
            try? await Task.sleep(for: .milliseconds(50))
            treesWhileStreaming = recorder.counts["get_work_tree"] ?? 0
        }
        XCTAssertGreaterThanOrEqual(
            treesWhileStreaming, 1,
            "the max-wait cap must refresh a board whose invalidations never pause"
        )
        XCTAssertLessThan(treesWhileStreaming, 14, "the stream is still coalesced")

        await waitForBurst(model, recorder)
        let settled = await model.waitForRefetchBurstsForTest()
        XCTAssertTrue(settled)
        XCTAssertGreaterThan(
            recorder.counts["get_work_tree"] ?? 0, treesWhileStreaming,
            "the trailing edge fires after the last trigger"
        )
    }

    // MARK: - Executions consumers

    func testExecutionsAreRefetchedOnlyForInvalidatedItemsWithAConsumer() async {
        let (model, recorder) = makeModel()
        openConsumers(model, ["task_1", "task_2"])
        recorder.reset()

        model.applyEventForTest(invalidation(itemIDs: ["task_1", "task_3"]))
        await waitForBurst(model, recorder)

        XCTAssertEqual(recorder.executionFetches, ["task_1"])
    }

    func testClosingTheLastConsumerStopsItsExecutionsRefetchAndEvictsTheHistory() async {
        let (model, recorder) = makeModel()
        let popover = UUID()
        let viewer = UUID()
        model.beginExecutionsConsumer(taskID: "task_1", token: popover)
        model.beginExecutionsConsumer(taskID: "task_1", token: viewer)
        model.applyEventForTest(.executionsList(taskId: "task_1", executions: []))
        XCTAssertNotNil(model.executionsByTaskID["task_1"])

        // Open: refetched.
        recorder.reset()
        model.applyEventForTest(invalidation(itemIDs: ["task_1"]))
        await waitForBurst(model, recorder)
        XCTAssertEqual(recorder.executionFetches, ["task_1"])

        // One of two consumers closes: still shown, still refetched.
        model.endExecutionsConsumer(taskID: "task_1", token: popover)
        XCTAssertNotNil(model.executionsByTaskID["task_1"])
        recorder.reset()
        model.backgroundWorkPending.removeAll()
        model.applyEventForTest(invalidation(itemIDs: ["task_1"]))
        await waitForBurst(model, recorder)
        XCTAssertEqual(recorder.executionFetches, ["task_1"])

        // The last one closes: evicted, and the invalidation no longer asks.
        model.endExecutionsConsumer(taskID: "task_1", token: viewer)
        XCTAssertNil(model.executionsByTaskID["task_1"], "history is dropped with its last consumer")
        recorder.reset()
        model.backgroundWorkPending.removeAll()
        model.applyEventForTest(invalidation(itemIDs: ["task_1"]))
        await waitForBurst(model, recorder)
        XCTAssertEqual(recorder.executionFetches, [], "a closed popover must stop costing a request per burst")
        XCTAssertEqual(recorder.counts["get_work_tree"], 1, "the rest of the burst is unaffected")
    }

    func testReplyArrivingAfterTheLastConsumerClosedDoesNotRecreateTheEntry() {
        let (model, _) = makeModel()
        let token = UUID()
        model.beginExecutionsConsumer(taskID: "task_1", token: token)
        model.endExecutionsConsumer(taskID: "task_1", token: token)

        model.applyEventForTest(.executionsList(taskId: "task_1", executions: []))

        XCTAssertNil(model.executionsByTaskID["task_1"])
    }

    func testEndingAnUnregisteredConsumerChangesNothing() {
        let (model, _) = makeModel()
        let token = UUID()
        model.beginExecutionsConsumer(taskID: "task_1", token: token)
        model.applyEventForTest(.executionsList(taskId: "task_1", executions: []))

        model.endExecutionsConsumer(taskID: "task_1", token: UUID())

        XCTAssertNotNil(model.executionsByTaskID["task_1"])
        XCTAssertTrue(model.hasOpenExecutionsConsumer(taskID: "task_1"))
    }

    // MARK: - Reuse by other triggers

    func testResyncRequiredUsesTheCoalescedBurstAndRefreshesEveryOpenConsumer() async {
        let (model, recorder) = makeModel()
        openConsumers(model, ["task_1", "task_2"])
        recorder.reset()

        model.applyEventForTest(invalidation(itemIDs: []))
        model.applyEventForTest(.resyncRequired)
        model.applyEventForTest(.resyncRequired)

        await waitForBurst(model, recorder, verb: "list_executions")

        XCTAssertEqual(recorder.counts["get_work_tree"], 1)
        XCTAssertEqual(recorder.counts["list_deferred_scope_attentions"], 1)
        XCTAssertNil(recorder.counts["list_planner_runs"])
        XCTAssertEqual(
            recorder.executionFetches, ["task_1", "task_2"],
            "a dropped invalidation could have covered any open history"
        )
    }

    func testInvalidationForAnotherProductSendsOnlyOffBoardData() async {
        let (model, recorder) = makeModel()
        openConsumers(model, ["task_b"])
        recorder.reset()

        model.applyEventForTest(.workInvalidated(topic: model.workTopic(forProductID: "prod_b"), productId: "prod_b", itemIds: ["task_b"]))
        await waitForBurst(model, recorder, verb: "list_executions")

        XCTAssertEqual(recorder.executionFetches, ["task_b"])
        XCTAssertEqual(recorder.counts["get_work_tree"], nil, "the board on screen is not refetched for another product")
        XCTAssertEqual(recorder.counts["list_planner_runs"], nil)
    }

    // MARK: - Helpers

    private final class Recorder {
        var sent: [[String: Any]] = []

        var counts: [String: Int] {
            sent.reduce(into: [:]) { counts, payload in
                counts[payload["type"] as? String ?? "?", default: 0] += 1
            }
        }

        var treeFetches: [String] {
            sent.filter { $0["type"] as? String == "get_work_tree" }.compactMap { $0["product_id"] as? String }
        }

        var executionFetches: [String] {
            sent.filter { $0["type"] as? String == "list_executions" }
                .compactMap { $0["work_item_id"] as? String }
                .sorted()
        }

        func reset() { sent.removeAll() }
    }

    private func invalidation(itemIDs: [String]) -> EngineEvent {
        .workInvalidated(topic: "work.product.prod_a", productId: "prod_a", itemIds: itemIDs)
    }

    /// Register a consumer per task, then forget the load requests that
    /// registering sends.
    private func openConsumers(_ model: ChatViewModel, _ taskIDs: [String]) {
        for taskID in taskIDs {
            model.beginExecutionsConsumer(taskID: taskID, token: UUID())
        }
    }

    /// Wait for the first burst to land, then for any straggler a second
    /// burst would have sent.
    private func waitForBurst(
        _ model: ChatViewModel,
        _ recorder: Recorder,
        verb: String = "get_work_tree",
        file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        let deadline = ContinuousClock.now + .seconds(5)
        while (recorder.counts[verb] ?? 0) == 0, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(5))
        }
        XCTAssertGreaterThan(recorder.counts[verb] ?? 0, 0, "no \(verb) arrived", file: file, line: line)
        try? await Task.sleep(for: model.refetchCoalescingPolicy.window * 3)
    }

    private func makeModel(
        policy: RefetchCoalescingPolicy = .init(window: .milliseconds(40), maxWait: .seconds(2))
    ) -> (ChatViewModel, Recorder) {
        let model = ChatViewModel(socketPath: "/tmp/boss-refetch-\(UUID().uuidString).sock")
        model.products = [makeProduct("prod_a"), makeProduct("prod_b")]
        model.selectWorkProduct("prod_a")
        model.projectsByProductID["prod_a"] = (1...3).map { makeProject("proj_a\($0)", productID: "prod_a") }
        model.isConnected = true
        model.engine.simulateConnectedForTesting = true
        model.refetchCoalescingPolicy = policy
        model.resyncRefetchGate = ResyncRefetchGate(policy: .init(
            window: policy.window, initialBackoff: .seconds(30), maxBackoff: .seconds(30), quietReset: .seconds(60)
        ))
        let recorder = Recorder()
        model.outboundRecorder = { recorder.sent.append($0) }
        return (model, recorder)
    }

    private func makeProduct(_ id: String) -> WorkProduct {
        WorkProduct(
            id: id,
            name: "Product \(id)",
            slug: id,
            description: "",
            repoRemoteURL: "https://github.com/org/repo.git",
            status: "active",
            createdAt: "2026-07-10T00:00:00Z",
            updatedAt: "2026-07-10T00:00:00Z"
        )
    }

    private func makeProject(_ id: String, productID: String) -> WorkProject {
        WorkProject(
            id: id,
            productID: productID,
            name: "Project \(id)",
            slug: id,
            description: "",
            goal: "",
            status: "active",
            priority: "medium",
            createdAt: "2026-07-10T00:00:00Z",
            updatedAt: "2026-07-10T00:00:00Z"
        )
    }
}
