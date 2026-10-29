import PaneLayout
import XCTest
@testable import Boss

@MainActor
final class AgentsGridStartupTests: XCTestCase {
    func testStartupArrivalsFillPagesBeforeAnyViewerAttaches() {
        for areaFirst in [false, true] {
            let grid = AgentsGridModel()
            if areaFirst { grid.noteArea(width: 1400, height: 900, inLiveResize: false) }
            for count in 1...14 {
                apply(count, to: grid)
                if areaFirst {
                    XCTAssertEqual(grid.layout.pageCount, (count + 5) / 6)
                }
            }
            if !areaFirst { grid.noteArea(width: 1400, height: 900, inLiveResize: false) }
            XCTAssertEqual(grid.layout.capacity, 6)
            XCTAssertEqual(grid.layout.pages.map(\.occupiedCount), [6, 6, 2])

            // A later real terminal measurement is an ordinary geometry boundary.
            grid.noteCellSize(width: 8, height: 16)
            XCTAssertEqual(grid.layout.capacity, 4)
            XCTAssertEqual(grid.layout.pages.map(\.occupiedCount), [4, 4, 4, 2])
            apply(15, to: grid)
            XCTAssertEqual(grid.layout.pages.map(\.occupiedCount), [4, 4, 4, 3])
        }
    }

    func testCellCallbackBeforeAreaDoesNotSizeAgainstInventedWindow() {
        let grid = AgentsGridModel()
        grid.noteCellSize(width: 6, height: 13)
        XCTAssertFalse(grid.layout.geometry.isMeasured)
        apply(14, to: grid)
        // Four columns fit within the dead band of the 1x1 fallback.
        // The first real area is initial geometry, not a resize.
        grid.noteArea(width: 1700, height: 720, inLiveResize: false)
        XCTAssertEqual(grid.layout.capacity, 8)
        XCTAssertEqual(grid.layout.pages.map(\.occupiedCount), [8, 6])
    }

    func testInvalidCellCallbacksKeepUsableStartupMetrics() {
        let grid = AgentsGridModel()
        grid.noteCellSize(width: .infinity, height: 13)
        grid.noteCellSize(width: 0, height: 0)
        grid.noteArea(width: 1400, height: 900, inLiveResize: false)
        XCTAssertEqual(grid.layout.capacity, 6)
        grid.noteCellSize(width: 8, height: 16)
        grid.noteCellSize(width: .nan, height: 16)
        XCTAssertEqual(grid.layout.capacity, 4)
    }

    private func apply(_ count: Int, to grid: AgentsGridModel) {
        let states = (0..<count).map { index in
            WorkerLiveState(
                slotId: index + 1, runId: "run-\(index)", model: "opus",
                shellPid: 1, lastEventAt: nil, currentTool: nil, lastToolEndedAt: nil,
                activity: .working, liveStatus: nil, liveStatusAt: nil,
                recoveryStatus: nil, tmuxHosted: true, agentType: "coding",
                hostId: WorkerLiveState.localHostId, startedAt: "2026-10-09T00:00:00Z", pool: "main"
            )
        }
        grid.apply(states: states, viewers: [:], interactivePoolSize: 16,
                   automationPoolSize: 8, reviewPoolSize: 16)
    }
}
