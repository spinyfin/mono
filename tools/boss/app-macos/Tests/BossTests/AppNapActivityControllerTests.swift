import Foundation
import XCTest
@testable import Boss

@MainActor
final class AppNapActivityControllerTests: XCTestCase {
    func testHoldsAssertionForTheProcessLifetime() {
        let counts = Counts()
        let controller = makeController(counts: counts)

        controller.beginForProcessLifetime()
        controller.beginForProcessLifetime()
        XCTAssertEqual(counts.began, 1)
        XCTAssertEqual(counts.ended, 0)

        controller.release()
        XCTAssertEqual(counts.ended, 1)
    }

    func testCanReacquireAfterTerminationRelease() {
        let counts = Counts()
        let controller = makeController(counts: counts)

        controller.beginForProcessLifetime()
        controller.release()
        controller.beginForProcessLifetime()

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
