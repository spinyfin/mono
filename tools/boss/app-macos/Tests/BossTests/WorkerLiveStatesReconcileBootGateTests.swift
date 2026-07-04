import XCTest
@testable import Boss

/// Covers `ChatViewModel`'s boot-id plumbing for the reconciliation
/// sweep. `WorkersWorkspaceModelTests` covers the actual per-pane
/// boot-id gate inside `reconcilePanes` (which must refuse to sweep a
/// pane spawned under an earlier engine boot than the one a given
/// snapshot came from, on the first reconnect after a restart OR any
/// later one). This file covers only that `ChatViewModel` always runs
/// the sweep and always forwards the snapshot's boot id via
/// `engineBootIdDidUpdate` BEFORE invoking `paneReconcileHandler`, so
/// the pane-side gate has the right baseline to compare against.
@MainActor
final class WorkerLiveStatesReconcileBootGateTests: XCTestCase {
    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    /// The sweep must run on every snapshot, including the first one
    /// after a detected engine restart — the actual protection against
    /// mass-killing live local panes lives in `reconcilePanes`'s
    /// per-pane boot-id comparison, not in skipping the sweep here.
    func testSweepsEveryReconnectRegardlessOfBootIdChange() {
        let model = makeModel()
        var sweepInvocations: [Set<String>] = []
        model.paneReconcileHandler = { liveRunIds in
            sweepInvocations.append(liveRunIds)
            return []
        }

        model.applyEventForTest(.connected)
        model.applyEventForTest(.workerLiveStatesList(states: [], engineProcessStartedAt: "2026-07-01T00:00:00Z"))
        XCTAssertEqual(sweepInvocations.count, 1, "the first-ever snapshot must run the sweep")

        // Engine restarts. Reconnect arms the sweep again, and the
        // snapshot that arrives is from the NEW process (different
        // boot id) — non-empty because a remote run reattached.
        model.applyEventForTest(.connected)
        model.applyEventForTest(.workerLiveStatesList(
            states: [makeLiveState(runId: "remote-run")],
            engineProcessStartedAt: "2026-07-03T00:00:00Z"
        ))
        XCTAssertEqual(
            sweepInvocations.count, 2,
            "the sweep must still run after a detected engine restart — the boot-id gate lives in reconcilePanes, not here"
        )
        XCTAssertEqual(sweepInvocations.last, ["remote-run"])

        // A SECOND reconnect to the SAME restarted engine boot must
        // also run the sweep (this is the scenario the gate used to
        // get wrong one layer down: same boot id twice in a row).
        model.applyEventForTest(.connected)
        model.applyEventForTest(.workerLiveStatesList(
            states: [makeLiveState(runId: "remote-run")],
            engineProcessStartedAt: "2026-07-03T00:00:00Z"
        ))
        XCTAssertEqual(sweepInvocations.count, 3)
    }

    /// `engineBootIdDidUpdate` must fire for every snapshot, with the
    /// snapshot's own boot id, before the sweep is invoked — so a pane
    /// allocator wired to it always has the right baseline in place by
    /// the time `paneReconcileHandler` runs.
    func testForwardsBootIdBeforeInvokingReconcileHandler() {
        let model = makeModel()
        var observedBootIds: [String] = []
        var bootIdAtSweepTime: String?
        model.engineBootIdDidUpdate = { bootId in
            observedBootIds.append(bootId)
        }
        model.paneReconcileHandler = { _ in
            bootIdAtSweepTime = observedBootIds.last
            return []
        }

        model.applyEventForTest(.connected)
        model.applyEventForTest(.workerLiveStatesList(states: [], engineProcessStartedAt: "2026-07-01T00:00:00Z"))

        XCTAssertEqual(observedBootIds, ["2026-07-01T00:00:00Z"])
        XCTAssertEqual(bootIdAtSweepTime, "2026-07-01T00:00:00Z")

        model.applyEventForTest(.connected)
        model.applyEventForTest(.workerLiveStatesList(states: [], engineProcessStartedAt: "2026-07-03T00:00:00Z"))

        XCTAssertEqual(observedBootIds, ["2026-07-01T00:00:00Z", "2026-07-03T00:00:00Z"])
        XCTAssertEqual(bootIdAtSweepTime, "2026-07-03T00:00:00Z")
    }

    private func makeLiveState(runId: String) -> WorkerLiveState {
        WorkerLiveState(
            slotId: 1,
            runId: runId,
            model: "claude",
            shellPid: 1234,
            lastEventAt: "2026-07-03T00:00:00Z",
            currentTool: nil,
            lastToolEndedAt: nil,
            activity: .working,
            liveStatus: nil,
            liveStatusAt: nil,
            recoveryStatus: nil
        )
    }
}
