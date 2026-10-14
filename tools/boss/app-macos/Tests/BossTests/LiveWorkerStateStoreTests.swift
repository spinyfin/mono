import Combine
import XCTest
@testable import Boss

@MainActor
final class LiveWorkerStateStoreTests: XCTestCase {
    func testIdleWorkerCountsAsAlive() {
        let store = LiveWorkerStateStore()

        store.update(states: [state(activity: .idle)])

        XCTAssertEqual(store.activeAgentCount, 1)
    }

    func testEqualSnapshotDoesNotRepublishByRunID() {
        let store = LiveWorkerStateStore()
        var publishes = 0
        let cancellable = store.$byRunID
            .dropFirst()
            .sink { _ in publishes += 1 }

        let states = [state(activity: .working)]
        store.update(states: states)
        store.update(states: states)

        XCTAssertEqual(publishes, 1)
        withExtendedLifetime(cancellable) {}
    }

    func testDuplicateRunIdsDoNotReplaceSnapshot() {
        let store = LiveWorkerStateStore()
        store.update(states: [state(activity: .working)])

        store.update(states: [
            state(slotId: 1, runId: "run-1", activity: .working),
            state(slotId: 2, runId: "run-1", activity: .idle),
        ])

        XCTAssertEqual(store.bySlot.count, 1)
        XCTAssertEqual(store.byRunID.count, 1)
        XCTAssertEqual(store.activeAgentCount, 1)
        XCTAssertEqual(store.bySlot[1]?.activity, .working)
    }

    func testDuplicateSlotIdsDoNotReplaceSnapshot() {
        let store = LiveWorkerStateStore()
        store.update(states: [state(activity: .working)])

        store.update(states: [
            state(slotId: 1, runId: "run-1", activity: .working),
            state(slotId: 1, runId: "run-2", activity: .idle),
        ])

        XCTAssertEqual(store.bySlot.count, 1)
        XCTAssertEqual(store.byRunID.count, 1)
        XCTAssertEqual(store.activeAgentCount, 1)
        XCTAssertEqual(store.byRunID["run-1"]?.activity, .working)
    }

    func testEmptySnapshotIsAuthoritativeAndClearsMembership() {
        let store = LiveWorkerStateStore()
        store.update(states: [state(activity: .working)])
        store.update(states: [])
        XCTAssertTrue(store.byRunID.isEmpty)
        XCTAssertTrue(store.bySlot.isEmpty)
        XCTAssertEqual(store.activeAgentCount, 0)
        XCTAssertFalse(store.feedUnavailable)
    }

    func testRejectedDuplicateDoesNotClearPriorSnapshot() {
        let store = LiveWorkerStateStore()
        store.update(states: [state(activity: .working)])
        store.markFeedUnavailable()
        XCTAssertTrue(store.feedUnavailable)

        store.update(states: [
            state(slotId: 1, runId: "run-1", activity: .idle),
            state(slotId: 2, runId: "run-1", activity: .working),
        ])

        XCTAssertEqual(store.byRunID["run-1"]?.activity, .working)
        XCTAssertTrue(store.feedUnavailable, "a rejected snapshot must not count as reconnection")
    }

    func testAcceptedSnapshotClearsFeedUnavailable() {
        let store = LiveWorkerStateStore()
        store.update(states: [state(activity: .working)])
        store.markFeedUnavailable()
        store.update(states: [state(activity: .idle)])
        XCTAssertFalse(store.feedUnavailable)
        XCTAssertEqual(store.byRunID["run-1"]?.activity, .idle)
    }

    func testMarkFeedUnavailableDoesNotClearMembership() {
        let store = LiveWorkerStateStore()
        store.update(states: [state(activity: .working)])
        store.markFeedUnavailable()
        XCTAssertEqual(store.byRunID.count, 1)
        XCTAssertEqual(store.bySlot.count, 1)
        XCTAssertTrue(store.feedUnavailable)
    }

    private func state(
        slotId: Int = 1,
        runId: String = "run-1",
        activity: WorkerActivity
    ) -> WorkerLiveState {
        WorkerLiveState(
            slotId: slotId,
            runId: runId,
            model: "model",
            shellPid: 1,
            lastEventAt: nil,
            currentTool: nil,
            lastToolEndedAt: nil,
            activity: activity,
            liveStatus: nil,
            liveStatusAt: nil,
            recoveryStatus: nil,
            tmuxHosted: nil
        )
    }
}
