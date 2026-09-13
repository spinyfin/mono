import XCTest
@testable import Boss

/// Coverage for the two guards this fix restores: `WorkerLiveState`'s
/// custom `==`/`hash(into:)` (excluding `lastEventAt`/`lastToolEndedAt`),
/// and `LiveWorkerStateStore.update` skipping the publish when the new
/// snapshot is otherwise value-equal to the previous one. Without both,
/// a hook event that advances only a timestamp still invalidates every
/// view observing the store.
final class WorkerLiveStateDedupTests: XCTestCase {
    func testStatesDifferingOnlyInTimestampsCompareEqual() {
        let a = makeLiveState(lastEventAt: "2026-06-01T00:00:00Z", lastToolEndedAt: "2026-06-01T00:00:00Z")
        let b = makeLiveState(lastEventAt: "2026-06-01T00:00:05Z", lastToolEndedAt: "2026-06-01T00:00:05Z")

        XCTAssertEqual(a, b, "lastEventAt/lastToolEndedAt must be excluded from equality")
        XCTAssertEqual(a.hashValue, b.hashValue, "hash must agree with the excluding equality")
    }

    func testStatesDifferingInAnyOtherFieldCompareUnequal() {
        let base = makeLiveState(lastEventAt: "2026-06-01T00:00:00Z", lastToolEndedAt: nil)

        XCTAssertNotEqual(base, makeLiveState(activity: .idle, lastEventAt: "2026-06-01T00:00:00Z", lastToolEndedAt: nil))
        XCTAssertNotEqual(
            base,
            makeLiveState(currentTool: "Edit", lastEventAt: "2026-06-01T00:00:00Z", lastToolEndedAt: nil)
        )
        XCTAssertNotEqual(base, makeLiveState(slotId: 2, lastEventAt: "2026-06-01T00:00:00Z", lastToolEndedAt: nil))
    }

    @MainActor
    func testStoreDoesNotPublishForAValueEqualSnapshot() {
        let store = LiveWorkerStateStore()
        let first = makeLiveState(lastEventAt: "2026-06-01T00:00:00Z", lastToolEndedAt: nil)
        store.update(states: [first])
        let publishedBefore = store.bySlot

        // A heartbeat that only advances the timestamps — same shape as
        // what the engine now reports as `changed = false`.
        let heartbeat = makeLiveState(lastEventAt: "2026-06-01T00:00:30Z", lastToolEndedAt: nil)
        store.update(states: [heartbeat])

        XCTAssertEqual(
            store.bySlot[1]?.lastEventAt,
            publishedBefore[1]?.lastEventAt,
            "an equal-other-fields snapshot must not replace the published value, even though the source carried a newer timestamp"
        )
    }

    @MainActor
    func testStorePublishesForAMeaningfulChange() {
        let store = LiveWorkerStateStore()
        store.update(states: [makeLiveState(activity: .working, lastEventAt: "2026-06-01T00:00:00Z", lastToolEndedAt: nil)])

        store.update(states: [makeLiveState(activity: .idle, lastEventAt: "2026-06-01T00:00:30Z", lastToolEndedAt: nil)])

        XCTAssertEqual(store.bySlot[1]?.activity, .idle)
        XCTAssertEqual(store.bySlot[1]?.lastEventAt, "2026-06-01T00:00:30Z")
    }

    // MARK: - Helpers

    private func makeLiveState(
        slotId: Int = 1,
        activity: WorkerActivity = .working,
        currentTool: String? = nil,
        lastEventAt: String?,
        lastToolEndedAt: String?
    ) -> WorkerLiveState {
        WorkerLiveState(
            slotId: slotId,
            runId: "exec-1",
            model: "claude-opus-4-7",
            shellPid: 1234,
            lastEventAt: lastEventAt,
            currentTool: currentTool,
            lastToolEndedAt: lastToolEndedAt,
            activity: activity,
            liveStatus: nil,
            liveStatusAt: nil,
            recoveryStatus: nil,
            tmuxHosted: nil
        )
    }
}
