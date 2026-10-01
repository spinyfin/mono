import XCTest
@testable import PaneLayout

final class PaneLayoutNavigationTests: XCTestCase {
    private typealias F = Fixture

    private func navigatedModel() -> PaneLayoutModel {
        var model = F.model()
        model.updateMembers(F.members(0...9))
        model.setFocusedRun(F.runId(2))
        model.selectPage(1)
        XCTAssertEqual(model.selectedPage, 1)
        return model
    }

    func testIdenticalSnapshotPreservesExplicitPageWithOffPageFocus() {
        var model = navigatedModel()
        model.updateMembers(F.members(0...9))
        XCTAssertEqual(model.selectedPage, 1)
        XCTAssertEqual(model.focusedRunId, F.runId(2))
    }

    func testMetadataSnapshotPreservesExplicitPageWithOffPageFocus() {
        var model = navigatedModel()
        var snapshot = F.members(0...9)
        snapshot[3].isWaiting = true
        model.updateMembers(snapshot)
        XCTAssertEqual(model.selectedPage, 1)
        XCTAssertEqual(model.waitingCount, 1)

        snapshot[3].hasViewer = false
        model.updateMembers(snapshot)
        XCTAssertEqual(model.selectedPage, 1)
        XCTAssertEqual(model.pages[0].cells[3].content, .occupied(snapshot[3]))
        XCTAssertEqual(model.focusedRunId, F.runId(2))
    }

    func testReleaseWithoutArrivalPreservesExplicitPageWithOffPageFocus() {
        var model = navigatedModel()
        model.updateMembers(F.members(0...9).filter { $0.runId != F.runId(3) })
        XCTAssertEqual(model.selectedPage, 1)
        XCTAssertEqual(model.pageCount, 2)
        XCTAssertEqual(model.pages[0].cells[3].content, .placeholder(runId: F.runId(3)))
        XCTAssertEqual(model.focusedRunId, F.runId(2))
    }
}
