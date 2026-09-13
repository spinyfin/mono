import Foundation
import XCTest
@testable import Boss

@MainActor
final class AppNapActivityControllerTests: XCTestCase {
    func testHoldsAssertionUntilAnIdleWorkerSnapshotArrives() {
        let counts = Counts()
        let controller = makeController(counts: counts)

        controller.beginUntilWorkerStateIsKnown()
        XCTAssertEqual(counts.began, 1)
        XCTAssertEqual(counts.ended, 0)

        controller.setWorkersActive(false)
        XCTAssertEqual(counts.ended, 1)
    }

    func testKeepsOneAssertionWhileWorkersRemainActiveThenReleasesIt() {
        let counts = Counts()
        let controller = makeController(counts: counts)

        controller.setWorkersActive(true)
        controller.setWorkersActive(true)
        XCTAssertEqual(counts.began, 1)

        controller.setWorkersActive(false)
        controller.release()
        XCTAssertEqual(counts.ended, 1)
    }

    func testReacquiresAfterWorkersStartAgain() {
        let counts = Counts()
        let controller = makeController(counts: counts)

        controller.setWorkersActive(true)
        controller.setWorkersActive(false)
        controller.setWorkersActive(true)

        XCTAssertEqual(counts.began, 2)
        XCTAssertEqual(counts.ended, 1)
    }

    private func makeController(counts: Counts) -> AppNapActivityController {
        AppNapActivityController(
            beginActivity: {
                counts.began += 1
                return NSObject()
            },
            endActivity: { _ in counts.ended += 1 }
        )
    }

    private final class Counts {
        var began = 0
        var ended = 0
    }
}
