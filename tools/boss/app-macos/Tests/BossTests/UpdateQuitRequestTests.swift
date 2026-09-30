import Foundation
import UpdateCore
import XCTest
@testable import Boss

@MainActor
final class UpdateQuitRequestTests: XCTestCase {
    func testQuitWaitsForSheetDismissalThenHandsOffRelaunch() throws {
        let previousPlan = UpdateLifecycle.pendingRelaunch
        defer { UpdateLifecycle.pendingRelaunch = previousPlan }
        let plan = try makePlan()
        UpdateLifecycle.pendingRelaunch = plan
        var events: [String] = []
        var relaunchPlan: SwapPlan?
        let request = UpdateQuitRequest {
            events.append("terminate")
            // The confirmed-quit delegate consumes this plan to arm the helper.
            relaunchPlan = UpdateLifecycle.consumePendingRelaunch()
        }

        request.request { events.append("dismiss") }
        XCTAssertEqual(events, ["dismiss"])
        XCTAssertNil(relaunchPlan)
        XCTAssertEqual(UpdateLifecycle.pendingRelaunch, plan)

        request.didDismiss()
        XCTAssertEqual(events, ["dismiss", "terminate"])
        XCTAssertEqual(relaunchPlan, plan)
        XCTAssertTrue(try XCTUnwrap(relaunchPlan).relaunch)
        XCTAssertNil(UpdateLifecycle.pendingRelaunch)
        request.didDismiss()
        XCTAssertEqual(events.count, 2, "a dismissal must not request termination twice")
    }

    func testCancelledQuitRetainsPlanAndCanBeRetried() throws {
        let previousPlan = UpdateLifecycle.pendingRelaunch
        defer { UpdateLifecycle.pendingRelaunch = previousPlan }
        let plan = try makePlan()
        UpdateLifecycle.pendingRelaunch = plan
        var terminationRequests = 0
        // Returning without applicationWillTerminate models Cancel in the quit alert.
        let request = UpdateQuitRequest { terminationRequests += 1 }
        request.request(dismiss: {})
        request.didDismiss()
        XCTAssertEqual(terminationRequests, 1)
        XCTAssertEqual(UpdateLifecycle.pendingRelaunch, plan)
        request.request(dismiss: {})
        request.didDismiss()
        XCTAssertEqual(terminationRequests, 2)
        XCTAssertEqual(UpdateLifecycle.pendingRelaunch, plan)
    }

    func testLaterOrSkipDismissalDoesNotQuit() {
        var terminationRequests = 0
        let request = UpdateQuitRequest { terminationRequests += 1 }
        request.didDismiss()
        XCTAssertEqual(terminationRequests, 0)
    }

    private func makePlan() throws -> SwapPlan {
        SwapPlan(
            version: try XCTUnwrap(VersionTuple.parse("1.0.691")),
            stagedBundleURL: URL(fileURLWithPath: "/tmp/update-test/staged/Boss.app"),
            installBundleURL: URL(fileURLWithPath: "/tmp/update-test/Boss.app"),
            backupURL: URL(fileURLWithPath: "/tmp/update-test/Boss.app.bak"),
            relaunch: true)
    }
}
