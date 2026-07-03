import XCTest
@testable import Boss

/// Covers the boot-identity gate on `ChatViewModel`'s reconciliation
/// sweep. `WorkersWorkspaceModelTests` already covers the plain
/// "empty snapshot" guard inside `reconcilePanes` itself, but that
/// guard alone does not protect a mixed local+remote engine restart:
/// reattach on engine startup only re-populates *remote* runs, so the
/// first post-restart `worker_live_states_list` snapshot can be
/// non-empty (it lists live remote runs) while still omitting every
/// locally-hosted run — the empty-only guard does not fire, and
/// reconciling against that snapshot would read as "every local pane
/// is dead". `ChatViewModel` closes this gap one layer up by comparing
/// `engine_process_started_at` across snapshots and skipping the sweep
/// entirely when it changes.
@MainActor
final class WorkerLiveStatesReconcileBootGateTests: XCTestCase {
    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    /// Simulates the exact mixed-restart scenario: the engine restarts
    /// while a local pane is still hosting real work and a remote
    /// worker is also live. The post-restart snapshot lists only the
    /// remote run id (as the current engine truly would), under a new
    /// `engine_process_started_at`. The sweep must be skipped for that
    /// snapshot — the still-live local run must not be reported as a
    /// pane to vacate.
    func testSkipsSweepWhenEngineBootIdChangesEvenWithNonEmptySnapshot() {
        let model = makeModel()
        var sweepInvocations: [Set<String>] = []
        model.paneReconcileHandler = { liveRunIds in
            sweepInvocations.append(liveRunIds)
            return []
        }

        // First connect: establishes the baseline boot id. Sweeping
        // here is safe regardless (freshly launched app has no
        // real panes yet), and this is what seeds
        // `lastKnownEngineProcessStartedAt`.
        model.applyEventForTest(.connected)
        model.applyEventForTest(.workerLiveStatesList(states: [], engineProcessStartedAt: "2026-07-01T00:00:00Z"))
        XCTAssertEqual(sweepInvocations.count, 1, "the first-ever snapshot must still run the sweep")

        // Engine restarts. Reconnect arms the sweep again, and the
        // snapshot that arrives is from the NEW process (different
        // boot id) — non-empty because a remote run reattached, but
        // missing the still-live local run.
        model.applyEventForTest(.connected)
        model.applyEventForTest(.workerLiveStatesList(
            states: [makeLiveState(runId: "remote-run")],
            engineProcessStartedAt: "2026-07-03T00:00:00Z"
        ))

        XCTAssertEqual(
            sweepInvocations.count, 1,
            "a snapshot from a newly restarted engine must not run the sweep at all, even though it is non-empty"
        )
    }

    /// A reconnect to the SAME engine process (unchanged boot id) is
    /// the ordinary "lost teardown message" recovery path and must
    /// keep sweeping as before.
    func testStillSweepsOnReconnectToTheSameEngineBoot() {
        let model = makeModel()
        var sweepInvocations: [Set<String>] = []
        model.paneReconcileHandler = { liveRunIds in
            sweepInvocations.append(liveRunIds)
            return []
        }

        model.applyEventForTest(.connected)
        model.applyEventForTest(.workerLiveStatesList(states: [], engineProcessStartedAt: "2026-07-01T00:00:00Z"))

        // A network blip reconnects to the very same engine process.
        model.applyEventForTest(.connected)
        model.applyEventForTest(.workerLiveStatesList(
            states: [makeLiveState(runId: "run-live")],
            engineProcessStartedAt: "2026-07-01T00:00:00Z"
        ))

        XCTAssertEqual(sweepInvocations.count, 2, "a reconnect to the same engine boot must still run the sweep")
        XCTAssertEqual(sweepInvocations.last, ["run-live"])
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
