import XCTest
@testable import Boss

/// `resync_required` means pushes were dropped, and the refetch it triggers
/// refills the queue that overflowed. These tests pin the three halves of the
/// answer: resyncs coalesce into one refetch set, repeated sets back off and
/// reset, and the last resync of a storm is always followed by a refetch.
/// See `ChatViewModel+ResyncRefetch.swift`.
///
/// The exponential schedule is tested on the gate with explicit instants, so
/// no assertion depends on a clock. The model-level tests use real timers
/// with short windows and wait for the requests rather than for a fixed time.
@MainActor
final class ResyncRefetchTests: XCTestCase {

    // MARK: - Gate: backoff schedule

    private let policy = ResyncRefetchPolicy(
        window: .milliseconds(100),
        initialBackoff: .seconds(1),
        maxBackoff: .seconds(8),
        quietReset: .seconds(5)
    )

    func testFirstResyncFiresAfterTheWindowOnly() {
        var gate = ResyncRefetchGate(policy: policy)
        let t0 = ContinuousClock.now

        XCTAssertTrue(gate.noteResync(at: t0))
        XCTAssertEqual(gate.delay(at: t0), .milliseconds(100))
    }

    func testResyncsWhilePendingAreAbsorbed() {
        var gate = ResyncRefetchGate(policy: policy)
        let t0 = ContinuousClock.now

        XCTAssertTrue(gate.noteResync(at: t0), "the first resync arms the timer")
        for step in 1...20 {
            XCTAssertFalse(
                gate.noteResync(at: t0 + .milliseconds(step)),
                "a resync with a fire pending must not arm another"
            )
        }
    }

    func testBackoffSequenceIsInitialThenDoublingThenCapped() {
        var gate = ResyncRefetchGate(policy: policy)
        var now = ContinuousClock.now
        var backoffs: [Duration] = []

        for _ in 0..<6 {
            XCTAssertTrue(gate.noteResync(at: now))
            backoffs.append(gate.currentBackoff)
            let fireAt = now + gate.delay(at: now)
            gate.noteFire(at: fireAt)
            now = fireAt + .milliseconds(1)
        }

        XCTAssertEqual(backoffs, [.seconds(1), .seconds(2), .seconds(4), .seconds(8), .seconds(8), .seconds(8)])
    }

    func testResyncDuringBackoffWaitsForItsEndAndThenFires() {
        var gate = ResyncRefetchGate(policy: policy)
        let t0 = ContinuousClock.now
        XCTAssertTrue(gate.noteResync(at: t0))
        gate.noteFire(at: t0)

        let t1 = t0 + .milliseconds(300)
        XCTAssertTrue(gate.noteResync(at: t1), "a resync after a fire is pending again, not lost")
        XCTAssertEqual(gate.delay(at: t1), .milliseconds(700), "it waits out the rest of the 1 s backoff")
    }

    // MARK: - Gate: reset after a quiet period

    func testQuietPeriodResetsTheBackoff() {
        var gate = ResyncRefetchGate(policy: policy)
        var now = ContinuousClock.now
        for _ in 0..<4 {
            XCTAssertTrue(gate.noteResync(at: now))
            let fireAt = now + gate.delay(at: now)
            gate.noteFire(at: fireAt)
            now = fireAt + .milliseconds(1)
        }
        XCTAssertEqual(gate.currentBackoff, .seconds(8))

        let afterQuiet = now + policy.quietReset
        XCTAssertTrue(gate.noteResync(at: afterQuiet))

        XCTAssertEqual(gate.currentBackoff, .seconds(1), "a quiet period starts the schedule over")
        XCTAssertEqual(gate.delay(at: afterQuiet), policy.window, "and the resync is answered after the window alone")
    }

    func testShortPauseDoesNotResetTheBackoff() {
        var gate = ResyncRefetchGate(policy: policy)
        var now = ContinuousClock.now
        for _ in 0..<3 {
            XCTAssertTrue(gate.noteResync(at: now))
            let fireAt = now + gate.delay(at: now)
            gate.noteFire(at: fireAt)
            now = fireAt + .milliseconds(1)
        }
        XCTAssertEqual(gate.currentBackoff, .seconds(8))

        XCTAssertTrue(gate.noteResync(at: now + policy.quietReset - .milliseconds(10)))

        XCTAssertEqual(gate.currentBackoff, .seconds(8), "a storm that is still being fed keeps its backoff")
    }

    func testCancelDropsThePendingFireWithoutCountingIt() {
        var gate = ResyncRefetchGate(policy: policy)
        let t0 = ContinuousClock.now
        XCTAssertTrue(gate.noteResync(at: t0))

        gate.cancel()

        XCTAssertFalse(gate.isDirty)
        XCTAssertEqual(gate.currentBackoff, .seconds(1))
        XCTAssertTrue(gate.noteResync(at: t0 + .milliseconds(5)), "the next resync arms a fresh timer")
    }

    // MARK: - Model: coalescing and set contents

    func testResyncsInOneWindowSendOneRefetchSet() async {
        let (model, recorder) = makeModel()

        for _ in 0..<25 {
            model.applyEventForTest(.resyncRequired)
        }
        XCTAssertTrue(recorder.sent.isEmpty, "nothing is sent while the window is open; got \(recorder.counts)")

        await waitForSet(model, recorder)

        XCTAssertEqual(
            recorder.counts,
            [
                "list_products": 1,
                "list_worker_live_states": 1,
                "get_engine_health": 1,
                "git_hub_auth_status": 1,
                "get_work_tree": 1,
                "list_deferred_scope_attentions": 1,
                "list_engine_attempts": 1,
            ],
            "25 resyncs must cost exactly one set, none of it per project"
        )
    }

    func testRefetchSetCoversEveryEvictablePushAndNothingMore() async {
        let (model, recorder) = makeModel()

        model.applyEventForTest(.resyncRequired)
        await waitForSet(model, recorder)

        for verb in ["list_products", "list_worker_live_states", "get_engine_health", "git_hub_auth_status", "get_work_tree"] {
            XCTAssertEqual(recorder.counts[verb], 1, "\(verb) re-reads state a dropped push could have covered")
        }
        for verb in ["list_live_status_disabled_slots", "list_planner_runs", "list_attention_groups", "list_attention_items_for_work_item"] {
            XCTAssertNil(recorder.counts[verb], "\(verb) is not state that a dropped push carries")
        }
    }

    func testProductsReplyDoesNotAddATreeFetch() async {
        let (model, recorder) = makeModel()

        model.applyEventForTest(.resyncRequired)
        await waitForSet(model, recorder)
        model.applyEventForTest(.productsList(products: model.products))
        let settled = await model.waitForRefetchBurstsForTest()

        XCTAssertTrue(settled)
        XCTAssertEqual(recorder.counts["get_work_tree"], 1)
    }

    func testOpenExecutionHistoriesAreRefetchedOnce() async {
        let (model, recorder) = makeModel()
        for taskID in ["task_1", "task_2"] { model.beginExecutionsConsumer(taskID: taskID, token: UUID()) }
        recorder.reset()

        model.applyEventForTest(.resyncRequired)
        model.applyEventForTest(.resyncRequired)
        await waitForSet(model, recorder)

        XCTAssertEqual(recorder.executionFetches, ["task_1", "task_2"])
    }

    // MARK: - Model: backoff and trailing edge

    func testSecondSetWaitsOutTheBackoffAndTrailingResyncIsNotLost() async {
        let backoff = Duration.seconds(1)
        let (model, recorder) = makeModel(policy: .init(
            window: .milliseconds(20), initialBackoff: backoff, maxBackoff: .seconds(5), quietReset: .seconds(60)
        ))

        model.applyEventForTest(.resyncRequired)
        await waitForSet(model, recorder)
        let firstSetAt = ContinuousClock.now
        XCTAssertEqual(recorder.counts["list_products"], 1)

        // A storm continues right after the first set: it must neither send
        // anything inside the backoff nor be dropped.
        for _ in 0..<10 { model.applyEventForTest(.resyncRequired) }
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(recorder.counts["list_products"], 1, "no second set inside the backoff")
        XCTAssertTrue(model.resyncRefetchGate.isDirty, "the resyncs are remembered")

        await waitUntil(recorder, verb: "list_products", count: 2)
        let secondSetAfter = firstSetAt.duration(to: .now)
        XCTAssertGreaterThanOrEqual(secondSetAfter, backoff - .milliseconds(300), "the trailing set honours the backoff")
        _ = await model.waitForRefetchBurstsForTest()
        XCTAssertEqual(recorder.counts["list_products"], 2, "ten resyncs in the backoff cost one trailing set")
        XCTAssertFalse(model.resyncRefetchGate.isDirty)
    }

    func testBackoffGrowsAcrossSetsInModel() async {
        let (model, recorder) = makeModel(policy: .init(
            window: .milliseconds(10), initialBackoff: .milliseconds(100), maxBackoff: .seconds(5), quietReset: .seconds(60)
        ))

        for expected in 1...3 {
            model.applyEventForTest(.resyncRequired)
            await waitUntil(recorder, verb: "list_products", count: expected)
        }
        _ = await model.waitForRefetchBurstsForTest()

        XCTAssertEqual(model.resyncRefetchGate.currentBackoff, .milliseconds(800), "three sets leave the next gap at 100 ms doubled three times")
    }

    func testStormEndsWithTheFinalStateRefetched() async {
        let (model, recorder) = makeModel(policy: .init(
            window: .milliseconds(10), initialBackoff: .milliseconds(80), maxBackoff: .seconds(5), quietReset: .seconds(60)
        ))

        for _ in 0..<30 {
            model.applyEventForTest(.resyncRequired)
            try? await Task.sleep(for: .milliseconds(5))
        }
        // The last resync lands inside a backoff; its refetch still has to come.
        let settled = await model.waitForRefetchBurstsForTest()
        XCTAssertTrue(settled)
        XCTAssertFalse(model.resyncRefetchGate.isDirty, "a storm never ends with an unserved resync")
        let sets = recorder.counts["list_products"] ?? 0
        XCTAssertGreaterThanOrEqual(sets, 2, "the storm outlasted the first backoff, so a trailing set was needed")
        XCTAssertLessThan(sets, 30, "the storm was coalesced")
    }

    func testDisconnectDropsThePendingRefetch() async {
        let (model, recorder) = makeModel()
        model.applyEventForTest(.resyncRequired)

        model.applyEventForTest(.disconnected)
        try? await Task.sleep(for: model.resyncRefetchGate.policy.window * 4)

        XCTAssertNil(recorder.counts["list_products"], "the next connect re-reads everything; nothing may be queued for it")
        XCTAssertFalse(model.resyncRefetchGate.isDirty)
    }

    // MARK: - Helpers

    private final class Recorder {
        var sent: [[String: Any]] = []

        var counts: [String: Int] {
            sent.reduce(into: [:]) { counts, payload in
                counts[payload["type"] as? String ?? "?", default: 0] += 1
            }
        }

        var executionFetches: [String] {
            sent.filter { $0["type"] as? String == "list_executions" }
                .compactMap { $0["work_item_id"] as? String }
                .sorted()
        }

        func reset() { sent.removeAll() }
    }

    /// Wait for the first set (it ends with the tree), then for any straggler
    /// a second set or burst would have sent.
    private func waitForSet(_ model: ChatViewModel, _ recorder: Recorder) async {
        await waitUntil(recorder, verb: "get_work_tree", count: 1)
        _ = await model.waitForRefetchBurstsForTest()
        try? await Task.sleep(for: model.refetchCoalescingPolicy.window * 3)
    }

    private func waitUntil(
        _ recorder: Recorder,
        verb: String,
        count: Int,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        let deadline = ContinuousClock.now + .seconds(5)
        while (recorder.counts[verb] ?? 0) < count, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(5))
        }
        XCTAssertGreaterThanOrEqual(recorder.counts[verb] ?? 0, count, "no \(verb) #\(count) arrived", file: file, line: line)
    }

    private func makeModel(
        policy: ResyncRefetchPolicy = .init(
            window: .milliseconds(30), initialBackoff: .seconds(30), maxBackoff: .seconds(30), quietReset: .seconds(60)
        )
    ) -> (ChatViewModel, Recorder) {
        let model = ChatViewModel(socketPath: "/tmp/boss-resync-\(UUID().uuidString).sock")
        model.products = [makeProduct("prod_a")]
        model.selectWorkProduct("prod_a")
        model.isConnected = true
        model.engine.simulateConnectedForTesting = true
        model.refetchCoalescingPolicy = .init(window: .milliseconds(20), maxWait: .seconds(2))
        model.resyncRefetchGate = ResyncRefetchGate(policy: policy)
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
}
