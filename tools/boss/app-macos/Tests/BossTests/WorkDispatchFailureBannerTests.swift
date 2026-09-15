import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class WorkDispatchFailureBannerTests: XCTestCase {
    func testParkNamesBothConditionsAndKeepsResumeVisible() throws {
        let diagnostic = "The sweep is holding this work item because its worker declared itself blocked; "
            + "it has ALSO produced 6 terminal executions, tripping the churn guard on top of the park. "
            + "Failing executions: " + String(repeating: "execution-id, ", count: 20)
        let banner = WorkDispatchFailureBanner(reason: "deliberate_park", errorText: diagnostic)
        XCTAssertEqual(banner.headline, "Waiting on you — deliberate park + churn guard")
        XCTAssertEqual(banner.summary, "Review the open attention item, then drag to Doing to resume.")

        // Capture at a narrow card width without creating a window or engine.
        let host = NSHostingView(rootView: banner.frame(width: 220).padding(8))
        host.frame = NSRect(origin: .zero, size: host.fittingSize)
        host.layoutSubtreeIfNeeded()
        let bitmap = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
        host.cacheDisplay(in: host.bounds, to: bitmap)
        let directory = try XCTUnwrap(ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"])
        let path = URL(fileURLWithPath: directory).appendingPathComponent("combined-park-banner.png")
        try XCTUnwrap(bitmap.representation(using: .png, properties: [:])).write(to: path)
    }

    func testParkOnlyAndDispatchFailureRemainDistinct() {
        let park = WorkDispatchFailureBanner(reason: "deliberate_park", errorText: "Worker declared blocked")
        XCTAssertEqual(park.headline, "Waiting on you — deliberate park")
        XCTAssertEqual(park.summary, "Review the open attention item, then drag to Doing to resume.")
        let failure = WorkDispatchFailureBanner(reason: "spawn_failed", errorText: "Could not launch worker")
        XCTAssertEqual(failure.headline, "Failed to start — spawn failed")
        XCTAssertEqual(failure.summary, "Could not launch worker")
    }
}
