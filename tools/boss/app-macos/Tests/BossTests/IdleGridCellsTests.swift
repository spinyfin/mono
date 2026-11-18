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

    /// 3×3 grid (min pane 10×10 in a 30×30 area) so occupancy can leave two
    /// empties (7) or fill the page (9).
    private func densePage(occupied: Int) throws -> PaneLayoutPage {
        let names = TrekPortraitGrid.workerNames
        let grid = AgentsGridModel(
            policy: PaneCapacityPolicy(
                minColumns: 1, minRows: 1, headerHeight: 0, maxPanesPerPage: 9, gap: 0, deadBand: 0
            ),
            isVisible: true
        )
        grid.noteCellSize(width: 10, height: 10)
        grid.noteArea(width: 30, height: 30, inLiveResize: false)
        grid.flushGeometryForTesting()
        grid.apply(
            states: (0..<occupied).map { makeState(runId: "r\($0)", slotId: $0 + 1, name: names[$0 % names.count]) },
            viewers: [:], interactivePoolSize: 16, automationPoolSize: 8,
            reviewPoolSize: 16, feedUnavailable: false
        )
        return try firstPage(grid)
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

    func testRemotePersonaIsNotChosenForIdleFiller() throws {
        let baseline = applied(["a", "b", "c"])
        let page = try firstPage(baseline)
        let before = baseline.idleCells(for: page)
        let taken = try XCTUnwrap(before.first).persona

        let grid = applied(["a", "b", "c"])
        let remote = WorkerLiveState(
            name: "\(taken) (Remote)", slotId: 9, runId: "r", model: "opus", shellPid: 1,
            lastEventAt: nil, currentTool: nil, lastToolEndedAt: nil, activity: .working,
            liveStatus: nil, liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true,
            agentType: "coding", hostId: "remote-host",
            startedAt: "2026-10-09T00:00:00Z", pool: "main"
        )
        grid.apply(
            states: ["a", "b", "c"].enumerated().map {
                makeState(runId: $1, slotId: $0 + 1, name: TrekPortraitGrid.workerNames[$0])
            } + [remote],
            viewers: [:], interactivePoolSize: 16, automationPoolSize: 8,
            reviewPoolSize: 16, feedUnavailable: false
        )
        let after = grid.idleCells(for: try firstPage(grid))
        XCTAssertFalse(after.map(\.persona).contains(taken))
        XCTAssertEqual(after.map(\.localIndex), before.map(\.localIndex))
        XCTAssertEqual(grid.layout.pages.flatMap(\.cells).filter { $0.content != .empty }.count, 3)
    }

    func testPlannerRunsOutOfCrewAndLeavesEmptyCellsBlank() throws {
        let grid = applied(["a", "b", "c"])
        let page = try firstPage(grid)
        XCTAssertTrue(page.cells.contains { $0.content == .empty })
        let cells = IdleGridCellPlanner.cells(
            for: page, busyPersonas: ["Kirk", "Spock"], seedRunIds: ["a"], crew: ["Kirk", "Spock"]
        )
        XCTAssertTrue(cells.isEmpty)
    }

    /// 3×3 page, seven occupied: two empties and a one-name crew fills the
    /// first and leaves the other blank (the planner `break`).
    func testPlannerLeavesRemainingEmptiesBlankWhenCrewRunsOut() throws {
        let page = try densePage(occupied: 7)
        let emptyIndices = page.cells.filter { $0.content == .empty }.map(\.localIndex)
        XCTAssertEqual(emptyIndices.count, 2)
        let cells = IdleGridCellPlanner.cells(
            for: page, busyPersonas: [], seedRunIds: ["r0"], crew: ["Kirk"]
        )
        XCTAssertEqual(cells.map(\.persona), ["Kirk"])
        XCTAssertEqual(cells.count, 1)
        XCTAssertTrue(emptyIndices.contains(cells[0].localIndex))
        XCTAssertEqual(Set(emptyIndices).subtracting([cells[0].localIndex]).count, 1)
    }

    func testFullPageYieldsNoIdleCells() throws {
        let page = try densePage(occupied: 9)
        XCTAssertFalse(page.cells.contains { $0.content == .empty })
        XCTAssertTrue(
            IdleGridCellPlanner.cells(
                for: page, busyPersonas: [], seedRunIds: ["r0"], crew: ["Kirk", "Spock"]
            ).isEmpty
        )
        let names = TrekPortraitGrid.workerNames
        let grid = AgentsGridModel(isVisible: true)
        grid.noteCellSize(width: 8, height: 16)
        grid.noteArea(width: 1200, height: 900, inLiveResize: false)
        grid.flushGeometryForTesting()
        grid.apply(
            states: (0..<60).map { makeState(runId: "r\($0)", slotId: $0 + 1, name: names[$0 % names.count]) },
            viewers: [:], interactivePoolSize: 16, automationPoolSize: 8,
            reviewPoolSize: 16, feedUnavailable: false
        )
        let packed = try firstPage(grid)
        XCTAssertFalse(packed.cells.contains { $0.content == .empty })
        XCTAssertTrue(grid.idleCells(for: packed).isEmpty)
    }

    func testChoiceMayChangeWhenAgentSetChanges() throws {
        let grid = applied(["a", "b", "c"])
        let page = try firstPage(grid)
        let crew = TrekPortraitGrid.workerNames
        let one = IdleGridCellPlanner.cells(for: page, busyPersonas: [], seedRunIds: ["a"], crew: crew)
        let changed = (0..<20).contains { n in
            IdleGridCellPlanner.cells(for: page, busyPersonas: [], seedRunIds: ["a", "n\(n)"], crew: crew) != one
        }
        XCTAssertTrue(changed)
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
