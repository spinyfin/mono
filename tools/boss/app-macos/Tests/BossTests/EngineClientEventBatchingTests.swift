import XCTest
import os
@testable import Boss

/// Regression guards for UI performance design entry 7: `EngineClient.emit`
/// accumulate-and-drain. A burst of N enqueued events must become one
/// main-actor drain turn delivering them in FIFO order, with
/// `UIUpdateCounters.recordEngineEventMainActor` counting on delivery
/// (not enqueue).
final class EngineClientEventBatchingTests: XCTestCase {
    func testBurstDeliversInOrderOnSingleDrainTurn() {
        let client = EngineClient(socketPath: "/tmp/boss-event-batch-\(UUID().uuidString).sock")
        let n = 50
        let delivered = OSAllocatedUnfairLock(initialState: [String]())
        let exp = expectation(description: "all events delivered")
        exp.expectedFulfillmentCount = n

        client.onEvent = { event in
            if case .error(let message) = event {
                delivered.withLock { $0.append(message) }
                exp.fulfill()
            }
        }

        // Synchronous burst before the main-actor drain can run — the
        // whole batch must collapse into one scheduled drain turn.
        for i in 0..<n {
            client.emitForTesting(.error(message: "\(i)"))
        }

        wait(for: [exp], timeout: 2)
        waitForDrainTurns(client, atLeast: 1)

        let messages = delivered.withLock { $0 }
        XCTAssertEqual(messages, (0..<n).map(String.init), "events must deliver in enqueue order")
        XCTAssertEqual(
            client.completedDrainTurnsForTesting(),
            1,
            "a synchronous burst must complete as a single main-actor drain turn"
        )
    }

    func testCounterRecordsOncePerDeliveredEvent() {
        // Use the process-wide shared counter; tests in a shard are
        // sequential, so a before/after delta equals this burst size.
        let before = UIUpdateCounters.shared.currentCounts().engineEventsMainActor

        let client = EngineClient(socketPath: "/tmp/boss-event-batch-counter-\(UUID().uuidString).sock")
        let n = 20
        let exp = expectation(description: "burst delivered")
        exp.expectedFulfillmentCount = n
        client.onEvent = { _ in exp.fulfill() }

        for i in 0..<n {
            client.emitForTesting(.error(message: "e\(i)"))
        }

        wait(for: [exp], timeout: 2)
        waitForDrainTurns(client, atLeast: 1)

        let after = UIUpdateCounters.shared.currentCounts().engineEventsMainActor
        XCTAssertEqual(
            after - before,
            UInt64(n),
            "recordEngineEventMainActor must fire once per delivered event"
        )
    }

    func testSecondBurstAfterDrainSchedulesNewTurn() {
        let client = EngineClient(socketPath: "/tmp/boss-event-batch-second-\(UUID().uuidString).sock")
        let first = expectation(description: "first event")
        let second = expectation(description: "second event")
        let count = OSAllocatedUnfairLock(initialState: 0)

        client.onEvent = { _ in
            let n = count.withLock { state -> Int in
                state += 1
                return state
            }
            if n == 1 { first.fulfill() }
            if n == 2 { second.fulfill() }
        }

        client.emitForTesting(.error(message: "a"))
        wait(for: [first], timeout: 2)
        waitForDrainTurns(client, atLeast: 1)
        XCTAssertEqual(client.completedDrainTurnsForTesting(), 1)

        client.emitForTesting(.error(message: "b"))
        wait(for: [second], timeout: 2)
        waitForDrainTurns(client, atLeast: 2)
        XCTAssertEqual(
            client.completedDrainTurnsForTesting(),
            2,
            "a later emit after the queue has drained must schedule a new turn"
        )
    }

    /// `onEvent` fulfills during the drain; the drain turn counter only
    /// advances after the empty-queue exit. Spin the run loop briefly so
    /// the main-actor task can finish before we assert on the counter.
    private func waitForDrainTurns(_ client: EngineClient, atLeast: UInt64) {
        let deadline = Date().addingTimeInterval(2)
        while client.completedDrainTurnsForTesting() < atLeast {
            if Date() > deadline {
                XCTFail("timed out waiting for completedDrainTurns >= \(atLeast)")
                return
            }
            RunLoop.current.run(until: Date().addingTimeInterval(0.01))
        }
    }
}
