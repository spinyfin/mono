import PaneLayout
import XCTest
@testable import Boss

@MainActor
final class IdleGridCellsTests: XCTestCase {
    private func applied(_ runs: [String]) -> AgentsGridModel {
        let grid = AgentsGridModel(isVisible: true)
        grid.noteCellSize(width: 8, height: 16)
        grid.noteArea(width: 1200, height: 900, inLiveResize: false)
        grid.flushGeometryForTesting()
        grid.apply(
            states: runs.enumerated().map { makeState(runId: $1, slotId: $0 + 1, name: TrekPortraitGrid.workerNames[$0]) },
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        return grid
    }

    private func firstPage(_ grid: AgentsGridModel) throws -> PaneLayoutPage {
        try XCTUnwrap(grid.layout.pages.first)
    }

    func testPartiallyFilledPageYieldsIdleCellsForTheRemainder() throws {
        let grid = applied(["a", "b", "c"])
        let page = try firstPage(grid)
        let empties = page.cells.filter { $0.content == .empty }.map(\.localIndex)
        XCTAssertFalse(empties.isEmpty)
        XCTAssertEqual(grid.idleCells(for: page).map(\.localIndex), empties)
    }

    func testIdlePersonasDoNotCollideWithActiveAgents() throws {
        let grid = applied(["a", "b", "c"])
        let page = try firstPage(grid)
        let active = Set(TrekPortraitGrid.workerNames.prefix(3))
        let idle = grid.idleCells(for: page).map(\.persona)
        XCTAssertTrue(active.isDisjoint(with: idle))
        XCTAssertEqual(Set(idle).count, idle.count)
    }

    func testChoiceIsStableAcrossRepeatedLayoutPasses() throws {
        let grid = applied(["a", "b", "c"])
        let first = grid.idleCells(for: try firstPage(grid))
        for _ in 0..<3 {
            grid.flushGeometryForTesting()
            grid.apply(
                states: [makeState(runId: "a", slotId: 1, name: TrekPortraitGrid.workerNames[0]),
                         makeState(runId: "b", slotId: 2, name: TrekPortraitGrid.workerNames[1]),
                         makeState(runId: "c", slotId: 3, name: TrekPortraitGrid.workerNames[2])],
                viewers: [:], interactivePoolSize: 16, automationPoolSize: 8,
                reviewPoolSize: 16, feedUnavailable: false
            )
            let page = try firstPage(grid)
            XCTAssertEqual(first.map(\.persona), grid.idleCells(for: page).map(\.persona))
        }
        let fresh = applied(["c", "b", "a"])
        XCTAssertEqual(first, fresh.idleCells(for: try firstPage(fresh)))
    }

    private func makeState(runId: String, slotId: Int, name: String) -> WorkerLiveState {
        WorkerLiveState(
            name: name, slotId: slotId, runId: runId, model: "opus", shellPid: 1,
            lastEventAt: nil, currentTool: nil, lastToolEndedAt: nil, activity: .working,
            liveStatus: nil, liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true,
            agentType: "coding", hostId: WorkerLiveState.localHostId,
            startedAt: "2026-10-09T00:00:00Z", pool: "main"
        )
    }
}
