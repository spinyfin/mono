import XCTest
@testable import PaneLayout

/// Epoch behaviour: stable logical cells, within-epoch growth, placeholders,
/// forced page collapse, pagination, and the anchor rule.
final class PaneLayoutModelEpochTests: XCTestCase {
    private typealias F = Fixture

    private func without(_ members: [PaneMember], _ removed: Int...) -> [PaneMember] {
        let ids = Set(removed.map(F.runId))
        return members.filter { !ids.contains($0.runId) }
    }

    // MARK: Empty and growth

    func testEmptyModelShowsEmptyState() throws {
        let model = F.model()
        XCTAssertEqual(model.capacity, 8)
        XCTAssertTrue(model.isEmpty)
        XCTAssertEqual(model.pageCount, 0)
        XCTAssertNil(model.currentPage)
        XCTAssertTrue(model.pages.isEmpty)
        XCTAssertFalse(model.selectorsVisible)
        XCTAssertTrue(model.pageSelectors.isEmpty)
    }

    func testOneToTwoGrowsTheGridWithoutMovingTheFirstRun() throws {
        var model = F.model()
        let first = F.member(0)
        model.setFocusedRun(first.runId)

        model.updateMembers([first])
        XCTAssertEqual(model.currentPage?.shape, GridShape(columns: 1, rows: 1))
        XCTAssertEqual(model.logicalCellIndex(ofRun: first.runId), 0)

        model.updateMembers([first, F.member(1)])
        let page = try XCTUnwrap(model.currentPage)
        XCTAssertEqual(page.shape, GridShape(columns: 2, rows: 1))
        XCTAssertEqual(F.tokens(page), [F.runId(0), F.runId(1)])
        XCTAssertEqual(model.logicalCellIndex(ofRun: first.runId), 0)
        XCTAssertEqual(model.anchorRunId, first.runId)
        XCTAssertEqual(model.pageCount, 1)
        XCTAssertFalse(model.selectorsVisible, "capacity 8 leaves 2-7 available without a selector")
    }

    func testGrowthKeepsEveryExistingRunInItsCell() throws {
        var model = F.model()
        var snapshot: [PaneMember] = []
        var indices: [String: Int] = [:]
        for n in 0..<8 {
            snapshot.append(F.member(n))
            model.updateMembers(snapshot)
            for (runId, index) in indices {
                XCTAssertEqual(model.logicalCellIndex(ofRun: runId), index, "\(runId) moved when \(n) arrived")
            }
            indices[F.runId(n)] = n
        }
        XCTAssertEqual(model.currentPage?.shape, GridShape(columns: 4, rows: 2))
    }

    func testInitialSnapshotIsPackedInStartOrderRegardlessOfInputOrder() throws {
        var model = F.model()
        let shuffled = [F.member(3), F.member(0), F.member(2), F.member(1)]
        model.updateMembers(shuffled)
        XCTAssertEqual(F.occupiedRunIds(try XCTUnwrap(model.currentPage)), (0...3).map(F.runId))
    }

    func testEqualStartTimesAreOrderedByRunId() throws {
        var model = F.model()
        let same = F.epoch
        let b = PaneMember(runId: "run-b", startedAt: same)
        let a = PaneMember(runId: "run-a", startedAt: same)
        model.updateMembers([b, a])
        XCTAssertEqual(F.occupiedRunIds(try XCTUnwrap(model.currentPage)), ["run-a", "run-b"])
    }

    // MARK: Stable cells, holes, placeholders

    func testReleaseLeavesAPlaceholderThatNeverCountsAsOccupied() throws {
        var model = F.model()
        let all = F.members(0...2)
        model.updateMembers(all)
        XCTAssertEqual(model.currentPage?.shape, GridShape(columns: 3, rows: 1))

        model.updateMembers(without(all, 1))
        let page = try XCTUnwrap(model.currentPage)
        XCTAssertEqual(F.tokens(page), [F.runId(0), "~" + F.runId(1), F.runId(2)])
        XCTAssertEqual(page.occupiedCount, 2)
        XCTAssertEqual(model.visibleCount, 2)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(2)), 2, "survivors keep their cells")
        XCTAssertTrue(model.isTidyHighlighted)

        model.tidy()
        XCTAssertEqual(F.tokens(try XCTUnwrap(model.currentPage)), [F.runId(0), F.runId(2)])
        XCTAssertEqual(model.currentPage?.shape, GridShape(columns: 2, rows: 1))
        XCTAssertFalse(model.isTidyHighlighted)
    }

    func testArrivalFillsLowestFreeCellEvenAPlaceholderAndEvenIfItIsOlder() throws {
        var model = F.model()
        let all = F.members(0...3)
        model.updateMembers(all)
        model.updateMembers(without(all, 1))

        let older = PaneMember(runId: "run-zz", startedAt: F.epoch.addingTimeInterval(-100))
        model.updateMembers(without(all, 1) + [older])
        XCTAssertEqual(model.logicalCellIndex(ofRun: "run-zz"), 1)
        XCTAssertEqual(F.tokens(try XCTUnwrap(model.currentPage)), [F.runId(0), "run-zz", F.runId(2), F.runId(3)])
    }

    func testRunKeepsItsCellAcrossOtherArrivalsReleasesAndMetadataRefreshes() throws {
        var model = F.model()
        let all = F.members(0...5)
        model.updateMembers(all)
        let anchorIndex = model.logicalCellIndex(ofRun: F.runId(4))

        var next = without(all, 1) + [F.member(6)]
        next[next.firstIndex { $0.runId == F.runId(4) }!].isWaiting = true
        model.updateMembers(next)

        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(4)), anchorIndex)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(6)), 1, "arrival takes the released cell")
        XCTAssertEqual(model.waitingCount, 1)
    }

    func testReappearingRunKeepsItsCellInsteadOfDuplicating() throws {
        var model = F.model()
        let all = F.members(0...2)
        model.updateMembers(all)
        model.updateMembers(without(all, 1))
        model.updateMembers(all)
        XCTAssertEqual(F.tokens(try XCTUnwrap(model.currentPage)), all.map(\.runId))
    }

    func testDuplicateRunIdsInASnapshotCollapseToOneMember() throws {
        var model = F.model()
        model.updateMembers([F.member(0), F.member(0, waiting: true)])
        XCTAssertEqual(model.visibleCount, 1)
        XCTAssertEqual(model.waitingCount, 1)
    }

    // MARK: Pagination

    func testNinthRunOpensASecondPageAndSelectorsAppear() throws {
        var model = F.model()
        model.updateMembers(F.members(0...7))
        XCTAssertEqual(model.pageCount, 1)
        XCTAssertFalse(model.selectorsVisible)

        model.updateMembers(F.members(0...8))
        XCTAssertEqual(model.pageCount, 2)
        XCTAssertTrue(model.selectorsVisible)
        XCTAssertEqual(model.pageSelectors.map(\.occupiedCount), [8, 1])
        XCTAssertEqual(model.selectedPage, 0, "an arrival never moves the reader")
        XCTAssertEqual(model.pages[1].shape, GridShape(columns: 1, rows: 1))
    }

    func testPageCountIsExactlyCeilOfMembersOverCapacity() throws {
        for count in 1...40 {
            var model = F.model()
            model.updateMembers(F.members(1...count))
            XCTAssertEqual(model.pageCount, (count + 7) / 8, "\(count) members")
            XCTAssertEqual(model.pages.map(\.occupiedCount).reduce(0, +), count)
            XCTAssertEqual(model.selectorsVisible, count > 8)
        }
    }

    func testSelectorsReportWaitingWorkersPerPage() throws {
        var model = F.model()
        var all = F.members(0...9)
        all[9].isWaiting = true
        model.updateMembers(all)
        XCTAssertEqual(model.pageSelectors.map(\.hasWaiting), [false, true])
        XCTAssertEqual(model.pageSelectors.map(\.waitingCount), [0, 1])
    }

    func testPlaceholdersCountTowardNeitherSelectorTotals() throws {
        var model = F.model()
        let all = F.members(0...9)
        model.updateMembers(all)
        model.updateMembers(without(all, 2))
        XCTAssertEqual(model.pageSelectors.map(\.occupiedCount), [7, 2])
    }

    func testSelectPageCompactsFirstThenSelectsAndClamps() throws {
        var model = F.model()
        let all = F.members(0...9)
        model.updateMembers(all)
        model.updateMembers(without(all, 2))
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(9)), 9)

        model.selectPage(1)
        XCTAssertEqual(model.selectedPage, 1)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(9)), 8, "page change is a boundary")
        XCTAssertFalse(model.isTidyHighlighted)

        model.selectPage(5)
        XCTAssertEqual(model.selectedPage, 1, "clamped to the highest page")
        model.selectPage(-3)
        XCTAssertEqual(model.selectedPage, 0)
    }

    func testReselectingTheCurrentPageIsNotABoundary() throws {
        var model = F.model()
        let all = F.members(0...9)
        model.updateMembers(all)
        model.updateMembers(without(all, 2))
        model.selectPage(0)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(9)), 9)
    }

    // MARK: Forced collapse

    func testNineToEightWithReaderOnPageTwoCollapsesAndKeepsTheReadersRun() throws {
        var model = F.model()
        let all = F.members(0...8)
        model.updateMembers(all)
        model.setFocusedRun(F.runId(8))
        model.selectPage(1)
        XCTAssertEqual(model.selectedPage, 1)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(8)), 8)

        model.updateMembers(without(all, 3))

        XCTAssertEqual(model.pageCount, 1)
        XCTAssertEqual(model.selectedPage, 0)
        XCTAssertFalse(model.selectorsVisible)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(8)), 7)
        XCTAssertEqual(model.anchorRunId, F.runId(8))
        XCTAssertEqual(model.focusedRunId, F.runId(8))
        XCTAssertFalse(model.isTidyHighlighted, "no hole is retained")
        XCTAssertEqual(
            F.tokens(try XCTUnwrap(model.currentPage)),
            [0, 1, 2, 4, 5, 6, 7, 8].map(F.runId)
        )
    }

    func testNineToEightWhenTheReadersOwnRunLeavesAnchorsOnTheNearestSurvivor() throws {
        var model = F.model()
        let all = F.members(0...8)
        model.updateMembers(all)
        model.setFocusedRun(F.runId(8))
        model.selectPage(1)

        model.updateMembers(without(all, 8))

        XCTAssertEqual(model.pageCount, 1)
        XCTAssertEqual(model.selectedPage, 0)
        XCTAssertFalse(model.selectorsVisible)
        XCTAssertEqual(model.anchorRunId, F.runId(7))
        XCTAssertTrue(F.tokens(try XCTUnwrap(model.currentPage)).allSatisfy { !$0.hasPrefix("~") })
    }

    func testNineToEightWithoutFocusUsesTheFirstSurvivingRunOnTheSelectedPage() throws {
        var model = F.model()
        let all = F.members(0...9)
        model.updateMembers(all)
        model.selectPage(1)
        // Page two holds runs 8 and 9. Release run 8 and a run on page one: 8 members remain.
        model.updateMembers(without(all, 8, 0))
        XCTAssertEqual(model.pageCount, 1)
        XCTAssertEqual(model.anchorRunId, F.runId(9))
        XCTAssertEqual(model.selectedPage, 0)
    }

    func testReleasedOverflowRunCollapsesImmediatelyEvenWhenAnotherHoleIsRetained() throws {
        var model = F.model()
        let all = F.members(0...8)
        model.updateMembers(all)
        model.updateMembers(without(all, 8))
        XCTAssertEqual(model.pageCount, 1, "a trailing page holding only a placeholder is removed")
    }

    func testHoleIsRetainedWhileMoreMembersThanOnePageRemain() throws {
        var model = F.model()
        let all = F.members(0...9)
        model.updateMembers(all)
        model.updateMembers(without(all, 2))
        XCTAssertEqual(model.pageCount, 2)
        XCTAssertTrue(model.selectorsVisible)
        XCTAssertEqual(F.tokens(model.pages[0])[2], "~" + F.runId(2))
    }

    func testSelectorsHideOnceReleasesLeaveASinglePageOfMembers() throws {
        var model = F.model()
        var current = F.members(0...16)
        model.updateMembers(current)
        XCTAssertEqual(model.pageCount, 3)

        current = without(current, 16)
        model.updateMembers(current)
        XCTAssertEqual(model.pageCount, 2, "two pages suffice for 16 members")
        XCTAssertTrue(model.selectorsVisible)

        for released in [2, 0, 1, 3, 4, 5, 6] {
            current = without(current, released)
            model.updateMembers(current)
            XCTAssertEqual(model.pageCount, 2, "15 down to 9 members still need two pages")
            XCTAssertTrue(model.selectorsVisible)
        }
        XCTAssertEqual(model.visibleCount, 9)

        current = without(current, 7)
        model.updateMembers(current)
        XCTAssertEqual(model.visibleCount, 8)
        XCTAssertEqual(model.pageCount, 1)
        XCTAssertFalse(model.selectorsVisible)
        XCTAssertEqual(model.pages.count, 1)
        XCTAssertFalse(model.isTidyHighlighted)
    }

    func testReleasingEveryRunClearsPlaceholdersAndShowsTheEmptyState() throws {
        var model = F.model()
        let all = F.members(0...2)
        model.updateMembers(all)
        model.updateMembers(without(all, 0, 2))
        XCTAssertEqual(model.pageCount, 1, "one run remains, so its placeholders stay")
        model.updateMembers([])
        XCTAssertTrue(model.isEmpty)
        XCTAssertNil(model.currentPage)
        XCTAssertEqual(model.selectedPage, 0)
        XCTAssertFalse(model.selectorsVisible)
        XCTAssertNil(model.anchorRunId)
    }

    func testReleasingTheLastRunsOfAMultiPageFleetDropsEveryOverflowPage() throws {
        var model = F.model()
        model.updateMembers(F.members(0...19))
        XCTAssertEqual(model.pageCount, 3)
        model.updateMembers([])
        XCTAssertEqual(model.pageCount, 0)
        XCTAssertTrue(model.pages.isEmpty)
    }

    // MARK: Anchor

    func testFocusedRunIsPreferredAsAnchorOverTheFirstRunOnThePage() throws {
        var model = F.model()
        let all = F.members(0...9)
        model.updateMembers(all)
        model.setFocusedRun(F.runId(9))
        model.updateMembers(without(all, 0))
        XCTAssertEqual(model.anchorRunId, F.runId(9))
    }

    func testWithoutFocusTheAnchorIsTheFirstRunOnTheSelectedPage() throws {
        var model = F.model()
        let all = F.members(0...9)
        model.updateMembers(all)
        model.updateMembers(without(all, 0))
        XCTAssertEqual(model.anchorRunId, F.runId(1))
    }

    // MARK: Visibility

    func testWhileHiddenChangesApplyImmediatelyWithoutPlaceholders() throws {
        var model = F.model(visible: false)
        let all = F.members(0...2)
        model.updateMembers(all)
        model.updateMembers(without(all, 1))
        XCTAssertEqual(F.tokens(try XCTUnwrap(model.currentPage)), [F.runId(0), F.runId(2)])
        XCTAssertFalse(model.isTidyHighlighted)
    }

    func testEnteringTheViewIsALayoutBoundary() throws {
        var model = F.model()
        let all = F.members(0...2)
        model.updateMembers(all)
        model.updateMembers(without(all, 1))
        XCTAssertTrue(model.isTidyHighlighted)

        model.setVisible(false)
        XCTAssertTrue(model.isTidyHighlighted, "hiding changes nothing")
        model.setVisible(true)
        XCTAssertFalse(model.isTidyHighlighted)
        XCTAssertEqual(F.tokens(try XCTUnwrap(model.currentPage)), [F.runId(0), F.runId(2)])
    }

    func testTidyKeepsTheFocusedRunOnTheSelectedPage() throws {
        var model = F.model()
        let all = F.members(0...9)
        model.updateMembers(all)
        model.setFocusedRun(F.runId(9))
        model.selectPage(1)
        model.updateMembers(without(all, 2))
        model.tidy()
        XCTAssertEqual(model.anchorRunId, F.runId(9))
        XCTAssertEqual(model.selectedPage, 1)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(9)), 8)
    }

    func testGrowthWithoutCompactionSelectsTheAnchorsPage() throws {
        var model = F.model()
        model.updateMembers(F.members(0...8))
        model.selectPage(1)
        XCTAssertEqual(model.selectedPage, 1)

        model.updateMembers(F.members(0...7) + [F.member(9), F.member(10)])
        XCTAssertEqual(model.anchorRunId, F.runId(7))
        XCTAssertEqual(model.selectedPage, 0)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(7)), 7)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(9)), 8)
        XCTAssertEqual(model.logicalCellIndex(ofRun: F.runId(10)), 9)
    }
}
