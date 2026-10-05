import XCTest
@testable import Boss

final class AIReviewStateBadgeTests: XCTestCase {
    func testOnlyCurrentHeadAllClearRendersGreen() {
        XCTAssertEqual(AIReviewStateBadge(state: "reviewed_all_clear").tint, .green)
        for state in ["not_reviewed", "reviewed_clean_pending", "reviewed_with_findings",
                      "reviewing", "review_queued", "review_not_required"] {
            XCTAssertNotEqual(AIReviewStateBadge(state: state).tint, .green, state)
        }
    }

    func testUnknownHeadAndCleanButNotReadyHaveDistinctExplanations() {
        let unreviewed = AIReviewStateBadge(state: "not_reviewed")
        XCTAssertEqual(unreviewed.systemImage, "questionmark.circle")
        XCTAssertTrue(unreviewed.tooltip.contains("no completed AI review"))
        let pending = AIReviewStateBadge(state: "reviewed_clean_pending")
        XCTAssertTrue(pending.tooltip.contains("AI review passed"))
        XCTAssertTrue(pending.tooltip.contains("prevent readiness"))
    }
}
