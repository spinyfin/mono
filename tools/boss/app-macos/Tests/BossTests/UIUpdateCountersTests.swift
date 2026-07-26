import XCTest
@testable import Boss

/// Covers the deterministic pieces of the UI-update counter subsystem:
/// atomic accumulation, exchange-on-flush, rate math, idle-flush zero
/// cost (returns nil, no ring append), and the ring.
/// The live 1 Hz timer and the production call-site wiring are out of
/// scope here — a sibling task wires the event paths.
final class UIUpdateCountersTests: XCTestCase {

    private func makeCounters(capacity: Int = 16) -> UIUpdateCounters {
        UIUpdateCounters(
            config: UIUpdateCounters.Config(flushIntervalSec: 1.0, capacity: capacity),
            nowNanos: { 0 },
            wallClockMs: { 1_700_000_000_000 }
        )
    }

    // MARK: - Accumulation

    func testIncrementsAccumulateIndependently() {
        let c = makeCounters()
        c.recordApplyWorkTree()
        c.recordApplyWorkTree()
        c.recordIncrementalTaskUpdate()
        c.recordEngineEventMainActor()
        c.recordEngineEventMainActor()
        c.recordEngineEventMainActor()
        c.recordCardBodyEvaluation()

        let counts = c.currentCounts()
        XCTAssertEqual(counts.applyWorkTree, 2)
        XCTAssertEqual(counts.incrementalTaskUpdates, 1)
        XCTAssertEqual(counts.engineEventsMainActor, 3)
        XCTAssertEqual(counts.cardBodyEvaluations, 1)
        XCTAssertFalse(counts.isEmpty)
    }

    func testFreshCountersAreEmpty() {
        let c = makeCounters()
        XCTAssertTrue(c.currentCounts().isEmpty)
    }

    // MARK: - Flush exchange + rates

    func testFlushExchangesCountsAndComputesRatesOverOneSecond() throws {
        let c = makeCounters()
        for _ in 0..<10 { c.recordApplyWorkTree() }
        for _ in 0..<20 { c.recordIncrementalTaskUpdate() }
        for _ in 0..<30 { c.recordEngineEventMainActor() }
        for _ in 0..<40 { c.recordCardBodyEvaluation() }

        let sample = try XCTUnwrap(
            c.flush(elapsedNanos: 1_000_000_000, tsEpochMs: 1_700_000_000_000)
        )

        XCTAssertEqual(sample.intervalMs, 1000.0, accuracy: 0.0001)
        XCTAssertEqual(sample.applyWorkTree, 10)
        XCTAssertEqual(sample.incrementalTaskUpdates, 20)
        XCTAssertEqual(sample.engineEventsMainActor, 30)
        XCTAssertEqual(sample.cardBodyEvaluations, 40)
        XCTAssertEqual(sample.applyWorkTreePerSec, 10.0, accuracy: 0.0001)
        XCTAssertEqual(sample.incrementalTaskUpdatesPerSec, 20.0, accuracy: 0.0001)
        XCTAssertEqual(sample.engineEventsMainActorPerSec, 30.0, accuracy: 0.0001)
        XCTAssertEqual(sample.cardBodyEvaluationsPerSec, 40.0, accuracy: 0.0001)

        // Exchange: counters reset so a follow-up flush with no new
        // increments is idle.
        XCTAssertTrue(c.currentCounts().isEmpty)
    }

    func testFlushRatesScaleWithElapsedInterval() throws {
        let c = makeCounters()
        for _ in 0..<100 { c.recordEngineEventMainActor() }

        // 100 events over 2 s → 50/s.
        let sample = try XCTUnwrap(
            c.flush(elapsedNanos: 2_000_000_000, tsEpochMs: 1)
        )
        XCTAssertEqual(sample.engineEventsMainActorPerSec, 50.0, accuracy: 0.0001)
        XCTAssertEqual(sample.intervalMs, 2000.0, accuracy: 0.0001)
    }

    func testFlushWithZeroElapsedDoesNotProduceInfiniteRates() throws {
        let c = makeCounters()
        c.recordApplyWorkTree()
        let sample = try XCTUnwrap(
            c.flush(elapsedNanos: 0, tsEpochMs: 1)
        )
        // TerminalLoopRate.perSecond returns 0 for a non-positive interval.
        XCTAssertEqual(sample.applyWorkTreePerSec, 0)
        XCTAssertEqual(sample.applyWorkTree, 1)
    }

    func testIdleFlushReturnsNilAndDoesNotTouchRing() {
        let c = makeCounters()
        XCTAssertNil(c.flush(elapsedNanos: 1_000_000_000, tsEpochMs: 1))
        XCTAssertTrue(c.snapshot().isEmpty)

        // Still idle after a prior exchange.
        c.recordApplyWorkTree()
        XCTAssertNotNil(c.flush(elapsedNanos: 1_000_000_000, tsEpochMs: 2))
        XCTAssertNil(c.flush(elapsedNanos: 1_000_000_000, tsEpochMs: 3))
        XCTAssertEqual(c.snapshot().count, 1)
    }

    func testFlushAppendsToRingAndPreservesOrder() throws {
        let c = makeCounters()
        c.recordApplyWorkTree()
        _ = c.flush(elapsedNanos: 1_000_000_000, tsEpochMs: 100)
        c.recordIncrementalTaskUpdate()
        _ = c.flush(elapsedNanos: 1_000_000_000, tsEpochMs: 200)

        let snap = c.snapshot()
        XCTAssertEqual(snap.count, 2)
        XCTAssertEqual(snap[0].tsEpochMs, 100)
        XCTAssertEqual(snap[0].applyWorkTree, 1)
        XCTAssertEqual(snap[1].tsEpochMs, 200)
        XCTAssertEqual(snap[1].incrementalTaskUpdates, 1)
    }

    func testRingEvictsOldestWhenOverCapacity() {
        let c = makeCounters(capacity: 2)
        for i in 0..<5 {
            c.recordApplyWorkTree()
            _ = c.flush(elapsedNanos: 1_000_000_000, tsEpochMs: Int64(i))
        }
        let snap = c.snapshot()
        XCTAssertEqual(snap.count, 2)
        XCTAssertEqual(snap.map(\.tsEpochMs), [3, 4])
    }

    // MARK: - Pure sample builder

    func testSampleBuilderReturnsNilForEmptyCounts() {
        XCTAssertNil(
            UIUpdateCounters.sample(
                from: .init(),
                elapsedNanos: 1_000_000_000,
                tsEpochMs: 1
            )
        )
    }

    func testSampleBuilderMapsSingleCounter() throws {
        var counts = UIUpdateCounters.Counts()
        counts.cardBodyEvaluations = 7
        let sample = try XCTUnwrap(
            UIUpdateCounters.sample(
                from: counts,
                elapsedNanos: 1_000_000_000,
                tsEpochMs: 42
            )
        )
        XCTAssertEqual(sample.tsEpochMs, 42)
        XCTAssertEqual(sample.cardBodyEvaluations, 7)
        XCTAssertEqual(sample.cardBodyEvaluationsPerSec, 7.0, accuracy: 0.0001)
        XCTAssertEqual(sample.applyWorkTree, 0)
        XCTAssertEqual(sample.applyWorkTreePerSec, 0, accuracy: 0.0001)
    }

    // MARK: - JSON contract

    func testSampleJSONUsesSnakeCase() throws {
        let sample = UIUpdateCounterSample(
            tsEpochMs: 1_700_000_000_000,
            intervalMs: 1000,
            applyWorkTreePerSec: 1,
            incrementalTaskUpdatesPerSec: 2,
            engineEventsMainActorPerSec: 3,
            cardBodyEvaluationsPerSec: 4,
            applyWorkTree: 1,
            incrementalTaskUpdates: 2,
            engineEventsMainActor: 3,
            cardBodyEvaluations: 4
        )
        let data = try JSONEncoder().encode(sample)
        let json = try XCTUnwrap(String(data: data, encoding: .utf8))
        XCTAssertTrue(json.contains("\"ts_epoch_ms\""))
        XCTAssertTrue(json.contains("\"apply_work_tree_per_sec\""))
        XCTAssertTrue(json.contains("\"incremental_task_updates_per_sec\""))
        XCTAssertTrue(json.contains("\"engine_events_main_actor_per_sec\""))
        XCTAssertTrue(json.contains("\"card_body_evaluations_per_sec\""))
        XCTAssertTrue(json.contains("\"apply_work_tree\""))

        let decoded = try JSONDecoder().decode(UIUpdateCounterSample.self, from: data)
        XCTAssertEqual(decoded, sample)
    }

    // MARK: - Concurrent increments

    func testConcurrentIncrementsAreNotLost() {
        let c = makeCounters()
        let group = DispatchGroup()
        let n = 1_000
        for _ in 0..<n {
            group.enter()
            DispatchQueue.global().async {
                c.recordApplyWorkTree()
                c.recordIncrementalTaskUpdate()
                c.recordEngineEventMainActor()
                c.recordCardBodyEvaluation()
                group.leave()
            }
        }
        XCTAssertEqual(group.wait(timeout: .now() + 5), .success)
        let counts = c.currentCounts()
        XCTAssertEqual(counts.applyWorkTree, UInt64(n))
        XCTAssertEqual(counts.incrementalTaskUpdates, UInt64(n))
        XCTAssertEqual(counts.engineEventsMainActor, UInt64(n))
        XCTAssertEqual(counts.cardBodyEvaluations, UInt64(n))
    }
}
