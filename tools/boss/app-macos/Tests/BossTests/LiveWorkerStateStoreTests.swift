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

    func testEqualSnapshotDoesNotRepublishSnapshotReceipt() {
        let store = LiveWorkerStateStore()
        var receiptChanges = 0
        let cancellable = store.$hasReceivedSnapshot
            .dropFirst()
            .sink { _ in receiptChanges += 1 }

        let states = [state(activity: .working)]
        store.update(states: states)
        store.update(states: states)

        XCTAssertEqual(receiptChanges, 1)
        withExtendedLifetime(cancellable) {}
    }

    private func state(activity: WorkerActivity) -> WorkerLiveState {
        WorkerLiveState(
            slotId: 1,
            runId: "run-1",
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
