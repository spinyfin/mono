import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class WorkDispatchFailureBannerTests: XCTestCase {
    func testParkNamesBothConditionsAndKeepsResumeVisible() throws {
        let diagnostic = "The sweep is holding this work item because its worker declared itself blocked; "
            + "it has ALSO produced 6 terminal executions, \(WorkDispatchFailureBanner.combinedParkChurnMarker). "
            + "Failing executions: " + String(repeating: "execution-id, ", count: 20)
        let banner = WorkDispatchFailureBanner(reason: "deliberate_park", errorText: diagnostic)
        XCTAssertEqual(banner.headline, "Waiting on you — deliberate park + churn guard")
        let summary = try XCTUnwrap(banner.summary)
        XCTAssertEqual(summary, "Review the open attention item, then drag to Doing to resume.")
        // Consumed by `body` as `.lineLimit(summaryLineLimit)`. A structurally
        // unlike control (full banner vs bare Text) cannot prove this: the
        // banner's icon, headline, and padding make it taller even under a
        // hard 1-line cap. Forcing `summaryLineLimit` to `1` makes this red.
        XCTAssertNil(
            banner.summaryLineLimit,
            "deliberate park must not cap the resume instruction"
        )
        XCTAssertEqual(
            WorkDispatchFailureBanner(reason: "spawn_failed", errorText: "Could not launch worker")
                .summaryLineLimit,
            3
        )
    }

    func testCancelledExecutionReadsAsStoppedNotFailed() {
        let text = "Execution `exec_1` was cancelled by an operator (explicit cancel). This work item was moved to "
            + "Backlog and will not restart on its own. Run `bossctl work start task_1` (or drag the card to Doing) "
            + "to resume it."
        let banner = WorkDispatchFailureBanner(reason: "execution_cancelled", errorText: text)
        XCTAssertEqual(banner.headline, "Stopped — execution cancelled")
        XCTAssertEqual(
            banner.summary, text,
            "the engine's stored actor, reason and resume instruction render verbatim — the app never guesses"
        )
        XCTAssertNil(banner.summaryLineLimit, "the resume instruction must not be capped")
    }

    func testParkOnlyAndDispatchFailureRemainDistinct() {
        let park = WorkDispatchFailureBanner(reason: "deliberate_park", errorText: "Worker declared blocked")
        XCTAssertEqual(park.headline, "Waiting on you — deliberate park")
        XCTAssertEqual(park.summary, "Review the open attention item, then drag to Doing to resume.")
        XCTAssertNil(park.summaryLineLimit)
        let failure = WorkDispatchFailureBanner(reason: "spawn_failed", errorText: "Could not launch worker")
        XCTAssertEqual(failure.headline, "Failed to start — spawn failed")
        XCTAssertEqual(failure.summary, "Could not launch worker")
        XCTAssertEqual(failure.summaryLineLimit, 3)
    }
}
