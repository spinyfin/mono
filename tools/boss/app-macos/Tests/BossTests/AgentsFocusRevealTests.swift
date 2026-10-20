import PaneLayout
import XCTest
@testable import Boss

@MainActor
final class AgentsFocusRevealTests: XCTestCase {
    private let smallPage = PaneCapacityPolicy(
        minColumns: 1, minRows: 1, headerHeight: 0, maxPanesPerPage: 4, gap: 0, deadBand: 0
    )

    private func makeGrid(runs: Int, visible: Bool = true) -> AgentsGridModel {
        let grid = AgentsGridModel(
            policy: smallPage,
            geometry: PaneGeometry(area: PaneSize(width: 200, height: 200), cell: PaneSize(width: 100, height: 100)),
            isVisible: visible
        )
        grid.apply(
            states: (0..<runs).map { index in
                makeState(
                    runId: "run-\(index)",
                    slotId: index + 1,
                    name: "Persona \(index)",
                    agentType: index == 0 ? "review" : "coding",
                    startedAt: "2026-10-09T00:00:\(String(format: "%02d", index))Z"
                )
            },
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        return grid
    }

    func testRevealSelectsOffPagePageAndFocusesRun() {
        let grid = makeGrid(runs: 6)
        XCTAssertEqual(grid.layout.selectedPage, 0)
        XCTAssertEqual(grid.reveal(runId: "run-5"), .revealed)
        XCTAssertEqual(grid.layout.selectedPage, 1)
        XCTAssertEqual(grid.layout.focusedRunId, "run-5")
        XCTAssertEqual(grid.highlightedRunId, "run-5")
        XCTAssertNil(grid.filterClearedBanner)
    }

    func testRevealClearsHidingFilterWithBanner() {
        let grid = makeGrid(runs: 3)
        grid.toggleType(.review)
        XCTAssertEqual(grid.layout.visibleCount, 1)
        XCTAssertEqual(grid.reveal(runId: "run-2"), .revealedClearingFilter)
        XCTAssertFalse(grid.layout.filter.isActive)
        XCTAssertEqual(grid.layout.visibleCount, 3)
        XCTAssertEqual(grid.layout.focusedRunId, "run-2")
        XCTAssertEqual(grid.filterClearedBanner, "Filter cleared to show Persona 2.")
    }

    func testRevealKeepsAFilterThatAlreadyAdmitsTheRun() {
        let grid = makeGrid(runs: 3)
        grid.toggleType(.coding)
        XCTAssertEqual(grid.reveal(runId: "run-1"), .revealed)
        XCTAssertTrue(grid.layout.filter.isActive)
    }

    func testRevealUnknownRunChangesNothing() {
        let grid = makeGrid(runs: 6)
        let before = grid.layout
        XCTAssertEqual(grid.reveal(runId: "ghost"), .notMember)
        XCTAssertEqual(grid.layout, before)
        XCTAssertNil(grid.highlightedRunId)
    }

    func testRevealWorksOnMemberWithoutViewer() {
        let grid = makeGrid(runs: 2)
        XCTAssertTrue(grid.unplacedViewers.isEmpty)
        XCTAssertEqual(grid.reveal(runId: "run-1"), .revealed)
        XCTAssertEqual(grid.highlightedRunId, "run-1")
    }

    func testWorkspaceRevealReportsUnknownRunAndSucceedsForViewerlessMember() {
        let workspace = WorkersWorkspaceModel()
        let grid = makeGrid(runs: 2, visible: false)
        let liveStates = LiveWorkerStateStore()
        WorkersDetailView.bindAgentReveal(grid: grid, workspace: workspace, liveStates: liveStates)
        // Membership arriving after binding must be used by the production handler.
        liveStates.update(states: [makeState(
            runId: "run-1", slotId: 2, name: "Persona 1", agentType: "coding",
            startedAt: "2026-10-09T00:00:01Z"
        )])
        guard case .success = workspace.revealAgent(runId: "run-1") else {
            return XCTFail("a viewer-less member is revealed as its card")
        }
        XCTAssertTrue(grid.layout.isVisible)
        XCTAssertEqual(grid.layout.focusedRunId, "run-1")
        guard case .failure(.unknownSlot) = workspace.revealAgent(runId: "run-0") else {
            return XCTFail("stale grid membership must be replaced before reveal")
        }
        guard case .failure(.unknownSlot) = workspace.revealAgent(runId: "ghost") else {
            return XCTFail("a run unknown to membership and viewers must fail")
        }
    }

    func testWorkspaceRevealWithoutHandlerFailsForUnknownRun() {
        guard case .failure(.unknownSlot) = WorkersWorkspaceModel().revealAgent(runId: "run-1") else {
            return XCTFail("expected unknownSlot")
        }
    }

    private func makeState(
        runId: String,
        slotId: Int,
        name: String?,
        agentType: String?,
        startedAt: String
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
            activity: .working,
            liveStatus: nil,
            liveStatusAt: nil,
            recoveryStatus: nil,
            tmuxHosted: true,
            agentType: agentType,
            projectId: nil,
            projectName: nil,
            hostId: WorkerLiveState.localHostId,
            startedAt: startedAt,
            pool: "main"
        )
    }
}
