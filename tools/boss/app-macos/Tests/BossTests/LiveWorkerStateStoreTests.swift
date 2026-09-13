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
