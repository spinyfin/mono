import XCTest
@testable import PaneLayout

/// Filters, cards without viewers, and geometry boundaries.
final class PaneLayoutModelBoundaryTests: XCTestCase {
    private typealias F = Fixture

    private let alpha = AgentProject.project(id: "alpha")
    private let beta = AgentProject.project(id: "beta")

    private func geometry(_ width: Double, _ height: Double) -> PaneGeometry {
        PaneGeometry(area: PaneSize(width: width, height: height), cell: PaneSize(width: 6, height: 13))
    }

    private func without(_ members: [PaneMember], _ removed: Int...) -> [PaneMember] {
        let ids = Set(removed.map(F.runId))
        return members.filter { !ids.contains($0.runId) }
    }

    /// Six members: reviews at 1 and 3, automation at 4, the rest coding;
    /// alpha owns 0-2, beta owns 3-4, 5 is unfiled.
    private func mixedFleet() -> [PaneMember] {
        [
            F.member(0, project: alpha),
            F.member(1, project: alpha, type: .review),
            F.member(2, project: alpha, waiting: true),
            F.member(3, project: beta, type: .review, waiting: true),
            F.member(4, project: beta, type: .automation),
            F.member(5, type: .unknown),
        ]
    }

    // MARK: Filters

    func testTypeFilterNarrowsMembershipAndPacksTheFilteredRuns() throws {
        var model = F.model()
        model.updateMembers(mixedFleet())
        model.setFilter(PaneFilter(types: [.review]))

        XCTAssertEqual(F.tokens(try XCTUnwrap(model.currentPage)), [F.runId(1), F.runId(3)])
        XCTAssertEqual(model.currentPage?.shape, GridShape(columns: 2, rows: 1))
        XCTAssertEqual(model.visibleCount, 2)
        XCTAssertEqual(model.hiddenCount, 4)
    }

    func testClearingTheFilterRestoresTheFullStartOrderedLayout() throws {
        var model = F.model()
        model.updateMembers(mixedFleet())
        model.setFilter(PaneFilter(projects: [beta]))
        model.setFilter(.all)
        XCTAssertEqual(F.tokens(try XCTUnwrap(model.currentPage)).prefix(6), ArraySlice((0...5).map(F.runId)))
        XCTAssertEqual(model.hiddenCount, 0)
    }

    func testProjectAndTypeFiltersCombine() throws {
        var model = F.model()
        model.updateMembers(mixedFleet())
        model.setFilter(PaneFilter(projects: [alpha, beta], types: [.review, .automation]))
        XCTAssertEqual(F.occupiedRunIds(try XCTUnwrap(model.currentPage)), [1, 3, 4].map(F.runId))
    }

    func testUnfiledAndUnknownAreFilterableCategories() throws {
        var model = F.model()
        model.updateMembers(mixedFleet())

        model.setFilter(PaneFilter(projects: [.unfiled]))
        XCTAssertEqual(F.occupiedRunIds(try XCTUnwrap(model.currentPage)), [F.runId(5)])

        model.setFilter(PaneFilter(types: [.unknown]))
        XCTAssertEqual(F.occupiedRunIds(try XCTUnwrap(model.currentPage)), [F.runId(5)])
    }

    func testFilterChangeEndsTheEpochAndDropsPlaceholdersAndHoles() throws {
        var model = F.model()
        let fleet = mixedFleet()
        model.updateMembers(fleet)
        model.updateMembers(without(fleet, 0))
        XCTAssertTrue(model.isTidyHighlighted)

        model.setFilter(PaneFilter(projects: [alpha, beta]))
        XCTAssertFalse(model.isTidyHighlighted)
        XCTAssertEqual(F.tokens(try XCTUnwrap(model.currentPage)), (1...4).map(F.runId))
    }

    func testEmptySelectionHidesEveryoneAndShowsTheEmptyState() {
        var model = F.model()
        model.updateMembers(mixedFleet())
        model.setFilter(PaneFilter(types: []))
        XCTAssertTrue(model.isEmpty)
        XCTAssertEqual(model.hiddenCount, 6)
        XCTAssertTrue(model.filter.isActive)
    }

    func testPaginationFollowsTheFilteredSetNotTheTotal() {
        var model = F.model()
        let fleet = (0...11).map { F.member($0, project: $0 < 3 ? alpha : beta) }
        model.updateMembers(fleet)
        XCTAssertEqual(model.pageCount, 2)
        model.setFilter(PaneFilter(projects: [alpha]))
        XCTAssertEqual(model.pageCount, 1)
        XCTAssertFalse(model.selectorsVisible)
        XCTAssertEqual(model.hiddenCount, 9)
        model.setFilter(.all)
        XCTAssertEqual(model.pageCount, 2)
        XCTAssertTrue(model.selectorsVisible)
    }

    func testWaitingCountsCoverVisibleAndFilteredOutWorkers() {
        var model = F.model()
        model.updateMembers(mixedFleet())
        XCTAssertEqual(model.waitingCount, 2)
        XCTAssertEqual(model.hiddenWaitingCount, 0)

        model.setFilter(PaneFilter(projects: [beta]))
        XCTAssertEqual(model.waitingCount, 1)
        XCTAssertEqual(model.hiddenWaitingCount, 1, "run 2 is waiting but filtered out")
    }

    func testFilterKeepsTheFocusedRunOnScreen() {
        var model = F.model()
        let fleet = (0...9).map { F.member($0, project: $0 == 0 || $0 == 9 ? alpha : beta) }
        model.updateMembers(fleet)
        model.setFocusedRun(F.runId(9))
        model.selectPage(1)

        model.setFilter(PaneFilter(projects: [alpha]))
        XCTAssertEqual(model.anchorRunId, F.runId(9))
        XCTAssertEqual(model.selectedPage, 0)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(9)), 1)
        XCTAssertFalse(model.selectorsVisible)
    }

    func testFilterThatHidesTheReadersRunAnchorsOnTheNearestSurvivor() {
        var model = F.model()
        let fleet = (0...9).map { F.member($0, type: $0 == 1 || $0 == 5 ? .review : .coding) }
        model.updateMembers(fleet)
        model.selectPage(1)

        model.setFilter(PaneFilter(types: [.review]))
        XCTAssertEqual(model.anchorRunId, F.runId(5), "nearest to run 8, the page's old first cell")
        XCTAssertEqual(model.selectedPage, 0)
    }

    func testMemberThatStopsMatchingLosesItsCellWithoutAPlaceholder() throws {
        var model = F.model()
        let reviews = [F.member(1, type: .review), F.member(2, type: .review)]
        model.updateMembers(reviews)
        model.setFilter(PaneFilter(types: [.review]))

        var changed = reviews
        changed[1].type = .coding
        model.updateMembers(changed)

        XCTAssertNil(model.logicalCellIndex(ofRun: F.runId(2)))
        // The vacated cell is a plain hole (the epoch keeps the 2x1 shape), not
        // a "finished" placeholder: the run is still running, just filtered out.
        XCTAssertEqual(F.tokens(try XCTUnwrap(model.currentPage)), [F.runId(1), "."])
        XCTAssertEqual(model.hiddenCount, 1)
    }

    func testFilteredArrivalOnlyAppearsWhenItMatches() {
        var model = F.model()
        model.setFilter(PaneFilter(types: [.design]))
        model.updateMembers([F.member(0), F.member(1, type: .design)])
        XCTAssertEqual(model.visibleCount, 1)
        XCTAssertEqual(model.hiddenCount, 1)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(1)), 0)
    }

    // MARK: Cards without viewers

    func testCardsWithoutViewersOccupyCapacityLikeAnyOtherMember() {
        var model = F.model()
        var fleet = F.members(0...8)
        for index in [0, 4, 8] { fleet[index].hasViewer = false }
        model.updateMembers(fleet)
        XCTAssertEqual(model.pageCount, 2, "nine members need two pages whether or not a viewer is attached")
        XCTAssertEqual(model.visibleCount, 9)
        XCTAssertEqual(model.pageSelectors.map(\.occupiedCount), [8, 1])
    }

    func testCardsWithoutViewersAreCarriedThroughToTheirCells() throws {
        var model = F.model()
        model.updateMembers([F.member(0, viewer: false), F.member(1)])
        let cells = try XCTUnwrap(model.currentPage).cells
        guard case .occupied(let card) = cells[0].content, case .occupied(let terminal) = cells[1].content else {
            return XCTFail("both cells should be occupied")
        }
        XCTAssertFalse(card.hasViewer)
        XCTAssertTrue(terminal.hasViewer)
    }

    func testViewerPresenceDoesNotChangeLayout() throws {
        var attached = F.model()
        var detached = F.model()
        let fleet = F.members(0...9)
        attached.updateMembers(fleet)
        detached.updateMembers(fleet.map { var member = $0; member.hasViewer = false; return member })
        XCTAssertEqual(attached.pages.map(F.occupiedRunIds), detached.pages.map(F.occupiedRunIds))
        XCTAssertEqual(attached.pages.map(\.shape), detached.pages.map(\.shape))
    }

    func testWaitingCardsWithoutViewersCountAndGetADot() {
        var model = F.model()
        var fleet = F.members(0...9)
        fleet[9].isWaiting = true
        fleet[9].hasViewer = false
        model.updateMembers(fleet)
        XCTAssertEqual(model.waitingCount, 1)
        XCTAssertEqual(model.pageSelectors.map(\.hasWaiting), [false, true])
    }

    func testViewerAttachingDoesNotMoveTheCell() {
        var model = F.model()
        var fleet = F.members(0...3)
        fleet[2].hasViewer = false
        model.updateMembers(fleet)
        fleet[2].hasViewer = true
        model.updateMembers(fleet)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(2)), 2)
    }

    func testACardWithoutAViewerCanBeTheAnchor() {
        var model = F.model()
        var fleet = F.members(0...8)
        fleet[8].hasViewer = false
        model.updateMembers(fleet)
        model.setFocusedRun(F.runId(8))
        model.selectPage(1)
        model.updateMembers(without(fleet, 3))
        XCTAssertEqual(model.anchorRunId, F.runId(8))
        XCTAssertEqual(model.selectedPage, 0)
    }

    func testFiltersApplyToCardsWithoutViewers() {
        var model = F.model()
        model.updateMembers([F.member(0, type: .review, viewer: false), F.member(1)])
        model.setFilter(PaneFilter(types: [.review]))
        XCTAssertEqual(model.visibleCount, 1)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(0)), 0)
    }

    // MARK: Geometry

    func testShrinkingCapacityRepacksIntoPagesAndKeepsTheFocusedRun() {
        var model = F.model()
        model.updateMembers(F.members(0...7))
        model.setFocusedRun(F.runId(6))

        model.updateGeometry(geometry(850, 720))

        XCTAssertEqual(model.capacity, 4)
        XCTAssertEqual(model.pageCount, 2)
        XCTAssertTrue(model.selectorsVisible)
        XCTAssertEqual(model.selectedPage, 1, "run 6 moved to the second page and the reader follows")
        XCTAssertEqual(model.anchorRunId, F.runId(6))
        XCTAssertEqual(model.pageSelectors.map(\.occupiedCount), [4, 4])
    }

    func testShrinkingCapacityWithoutFocusStaysOnTheSelectedPagesFirstRun() {
        var model = F.model()
        model.updateMembers(F.members(0...7))
        model.updateGeometry(geometry(850, 720))
        XCTAssertEqual(model.selectedPage, 0)
        XCTAssertEqual(model.anchorRunId, F.runId(0))
    }

    func testGrowingCapacityMergesPagesAndHidesSelectors() {
        var model = F.model()
        model.updateMembers(F.members(0...11))
        XCTAssertEqual(model.pageCount, 2)

        model.updateGeometry(geometry(2200, 1500))
        XCTAssertEqual(model.capacity, 16)
        XCTAssertEqual(model.pageCount, 1)
        XCTAssertFalse(model.selectorsVisible)
        XCTAssertEqual(model.selectedPage, 0)
    }

    func testSeventeenMembersNeedASecondPageEvenAtTheSixteenPaneCap() {
        var model = F.model(geometry: geometry(4000, 3000))
        model.updateMembers(F.members(0...16))
        XCTAssertEqual(model.capacity, 16)
        XCTAssertEqual(model.pageCount, 2)
        XCTAssertEqual(model.pageSelectors.map(\.occupiedCount), [16, 1])
    }

    func testResizeInsideTheDeadBandKeepsCapacity() {
        var model = F.model()
        model.updateGeometry(geometry(1690, 720))
        XCTAssertEqual(model.limits, GridLimits(columns: 4, rows: 2))
        XCTAssertEqual(model.capacity, 8)

        model.updateGeometry(geometry(1670, 720))
        XCTAssertEqual(model.limits, GridLimits(columns: 3, rows: 2))
        XCTAssertEqual(model.capacity, 6)
    }

    func testResizeEndIsALayoutBoundaryEvenWhenCapacityIsUnchanged() throws {
        var model = F.model()
        let fleet = F.members(0...2)
        model.updateMembers(fleet)
        model.updateMembers(without(fleet, 1))
        XCTAssertTrue(model.isTidyHighlighted)

        model.updateGeometry(geometry(1690, 720))
        XCTAssertFalse(model.isTidyHighlighted)
        XCTAssertEqual(F.tokens(try XCTUnwrap(model.currentPage)), [F.runId(0), F.runId(2)])
    }

    func testIdenticalGeometryIsANoOp() {
        var model = F.model()
        let fleet = F.members(0...2)
        model.updateMembers(fleet)
        model.updateMembers(without(fleet, 1))
        model.updateGeometry(F.laptop)
        XCTAssertTrue(model.isTidyHighlighted, "no boundary, so the placeholder is retained")
    }

    func testUnmeasuredCellSizeKeepsTheLastCapacity() {
        var model = F.model()
        model.updateGeometry(PaneGeometry(area: F.laptop.area, cell: PaneSize(width: 0, height: 0)))
        XCTAssertEqual(model.capacity, 8)
    }

    func testWindowBelowTheMinimumStillRendersOnePane() throws {
        var model = F.model(geometry: geometry(300, 200))
        model.updateMembers(F.members(0...2))
        XCTAssertEqual(model.capacity, 1)
        XCTAssertEqual(model.pageCount, 3)
        XCTAssertEqual(try XCTUnwrap(model.currentPage).shape, GridShape(columns: 1, rows: 1))
    }
}
