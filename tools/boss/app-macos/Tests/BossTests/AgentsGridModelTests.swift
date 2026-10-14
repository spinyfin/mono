import PaneLayout
import XCTest
@testable import Boss

@MainActor
final class AgentsGridModelTests: XCTestCase {
    func testRemoteWorkersAreExcludedFromMembership() {
        let grid = AgentsGridModel(isVisible: true)
        grid.apply(
            states: [
                makeState(runId: "local-1", hostId: WorkerLiveState.localHostId),
                makeState(runId: "remote-1", slotId: 200, hostId: "ssh-host"),
            ],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.visibleCount, 1)
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "local-1"), 0)
        XCTAssertNil(grid.layout.logicalCellIndex(ofRun: "remote-1"))
    }

    func testMissingHostIdDoesNotInventLocalMembership() {
        let grid = AgentsGridModel()
        grid.apply(
            states: [makeState(runId: "legacy", hostId: nil)],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertTrue(grid.layout.isEmpty)
    }

    func testAttachBeforeSnapshotDoesNotInventMembership() {
        let workspace = WorkersWorkspaceModel()
        _ = workspace.attachWorkerPane(makeAttach(runId: "early", slot: 1))
        let grid = AgentsGridModel()
        grid.apply(
            states: [],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertTrue(grid.layout.isEmpty)
        XCTAssertEqual(grid.unplacedViewers.map(\.runId), ["early"])
        XCTAssertNotNil(workspace.viewersByRunId["early"]?.session)
    }

    func testSnapshotThenAttachFillsTheSameCell() throws {
        let workspace = WorkersWorkspaceModel()
        let grid = AgentsGridModel()
        let state = makeState(runId: "run-a")
        grid.apply(
            states: [state],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.visibleCount, 1)
        XCTAssertTrue(grid.unplacedViewers.isEmpty)

        _ = workspace.attachWorkerPane(makeAttach(runId: "run-a", slot: 1))
        grid.apply(
            states: [state],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "run-a"), 0)
        let page = try XCTUnwrap(grid.layout.currentPage)
        guard case .occupied(let member) = page.cells[0].content else {
            XCTFail("expected occupied cell")
            return
        }
        XCTAssertTrue(member.hasViewer)
        XCTAssertTrue(grid.unplacedViewers.isEmpty)
    }

    func testEngineReleaseThenDelayedDetachDoesNotResurrectMembership() {
        let workspace = WorkersWorkspaceModel()
        _ = workspace.attachWorkerPane(makeAttach(runId: "run-a", slot: 1))
        let grid = AgentsGridModel()
        grid.apply(
            states: [makeState(runId: "run-a")],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.visibleCount, 1)

        grid.apply(
            states: [],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertTrue(grid.layout.isEmpty)
        XCTAssertEqual(grid.unplacedViewers.map(\.runId), ["run-a"])

        _ = workspace.detachWorkerPane(runId: "run-a")
        grid.apply(
            states: [],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertTrue(grid.layout.isEmpty)
        XCTAssertTrue(grid.unplacedViewers.isEmpty)
        XCTAssertTrue(workspace.viewersByRunId.isEmpty)
    }

    func testRejectedStoreSnapshotLeavesGridUnchanged() {
        let store = LiveWorkerStateStore()
        store.update(states: [makeState(runId: "run-a")])
        let grid = AgentsGridModel()
        grid.apply(
            states: Array(store.byRunID.values),
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        store.update(states: [
            makeState(runId: "run-a", slotId: 1),
            makeState(runId: "run-a", slotId: 2),
        ])
        grid.apply(
            states: Array(store.byRunID.values),
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: store.feedUnavailable
        )
        XCTAssertEqual(store.byRunID.count, 1)
        XCTAssertEqual(grid.layout.visibleCount, 1)
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "run-a"), 0)
    }

    func testFeedUnavailableRetainsLastSnapshot() {
        let store = LiveWorkerStateStore()
        store.update(states: [makeState(runId: "run-a", activity: .working)])
        store.markFeedUnavailable()
        let grid = AgentsGridModel()
        grid.apply(
            states: Array(store.byRunID.values),
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: store.feedUnavailable
        )
        XCTAssertTrue(grid.feedUnavailable)
        XCTAssertEqual(grid.layout.visibleCount, 1)

        store.update(states: [makeState(runId: "run-a", activity: .idle)])
        grid.apply(
            states: Array(store.byRunID.values),
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: store.feedUnavailable
        )
        XCTAssertFalse(grid.feedUnavailable)
        XCTAssertEqual(store.byRunID["run-a"]?.activity, .idle)
    }

    func testTerminatedMemberRendersAsExitedUntilRelease() {
        let grid = AgentsGridModel()
        grid.apply(
            states: [makeState(runId: "run-a", activity: .terminated)],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.visibleCount, 1)
        let live = makeState(runId: "run-a", activity: .terminated)
        let snapshot = WorkerSlotSnapshot.build(
            slot: WorkerSlot(slotId: 1, runId: "run-a"),
            liveState: live,
            liveStatusEnabled: true
        )
        XCTAssertTrue(snapshot.isExited)
        XCTAssertEqual(live.activity.agentsPaneLabel, "Exited")
        XCTAssertTrue(snapshot.isViewerMissing)
    }

    func testSpawningUsesStartingLabel() {
        XCTAssertEqual(WorkerActivity.spawning.agentsPaneLabel, "Starting")
        XCTAssertEqual(WorkerActivity.working.agentsPaneLabel, "Working")
    }

    func testUnknownAgentTypeStaysInTheGrid() throws {
        let grid = AgentsGridModel()
        grid.apply(
            states: [makeState(runId: "run-a", agentType: "time-travel")],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.visibleCount, 1)
        let page = try XCTUnwrap(grid.layout.currentPage)
        guard case .occupied(let member) = page.cells[0].content else {
            XCTFail("expected occupied cell")
            return
        }
        XCTAssertEqual(member.type, .unknown)
    }

    func testPoolStripUsesAttributedPoolNotSlotRange() {
        let strip = AgentsGridModel.poolStrip(
            states: [
                makeState(runId: "a", slotId: 9, pool: "automation"),
                makeState(runId: "b", slotId: 1, pool: "main"),
                makeState(runId: "c", slotId: 25, pool: "review"),
            ],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16
        )
        XCTAssertEqual(strip, "Interactive 1/16 · Automation 1/8 · Review 1/16")
    }

    func testLiveResizeDefersGeometryUntilEnd() {
        let grid = AgentsGridModel(
            geometry: PaneGeometry(
                area: PaneSize(width: 800, height: 600),
                cell: PaneSize(width: 6, height: 13)
            )
        )
        let before = grid.layout.capacity
        grid.noteArea(width: 400, height: 300, inLiveResize: true)
        XCTAssertEqual(grid.layout.capacity, before)
        grid.liveResizeDidEnd()
        XCTAssertNotEqual(grid.layout.geometry.area.width, 800)
    }

    func testPersonaSurvivesPlaceholderAfterRelease() {
        let grid = AgentsGridModel()
        grid.apply(
            states: [makeState(runId: "run-a", name: "Riker")],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.personaName(for: "run-a"), "Riker")
        // A second member keeps the page count from collapsing to empty
        // so a release can leave a placeholder instead of compacting away.
        grid.apply(
            states: [
                makeState(runId: "run-a", name: "Riker"),
                makeState(runId: "run-b", slotId: 2, name: "Data", startedAt: "2026-10-09T00:00:01Z"),
            ],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        grid.apply(
            states: [makeState(runId: "run-b", slotId: 2, name: "Data", startedAt: "2026-10-09T00:00:01Z")],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        // Forced collapse when fewer pages suffice removes placeholders.
        // Persona for the surviving run remains.
        XCTAssertEqual(grid.personaName(for: "run-b"), "Data")
    }

    func testAgentTypeFromWire() {
        XCTAssertEqual(AgentType.fromWire("coding"), .coding)
        XCTAssertEqual(AgentType.fromWire("review"), .review)
        XCTAssertEqual(AgentType.fromWire(nil), .unknown)
        XCTAssertEqual(AgentType.fromWire("time-travel"), .unknown)
        XCTAssertEqual(AgentType.unknown.badgeLabel, "Unknown")
    }

    private func makeAttach(runId: String, slot: Int) -> EngineAttachRequest {
        EngineAttachRequest(
            runId: runId,
            slotId: slot,
            sessionName: "boss-\(slot)-\(runId)",
            tmuxSocketPath: "/state/boss/tmux.sock",
            summary: nil,
            taskTitle: nil
        )
    }

    private func makeState(
        runId: String,
        slotId: Int = 1,
        name: String? = nil,
        activity: WorkerActivity = .working,
        agentType: String? = "coding",
        hostId: String? = WorkerLiveState.localHostId,
        startedAt: String = "2026-10-09T00:00:00Z",
        pool: String? = "main"
    ) -> WorkerLiveState {
        WorkerLiveState(
            name: name,
            slotId: slotId,
            runId: runId,
            model: "opus",
            shellPid: 1,
            lastEventAt: nil,
            currentTool: nil,
            lastToolEndedAt: nil,
            activity: activity,
            liveStatus: nil,
            liveStatusAt: nil,
            recoveryStatus: nil,
            tmuxHosted: true,
            agentType: agentType,
            hostId: hostId,
            startedAt: startedAt,
            pool: pool
        )
    }
}
