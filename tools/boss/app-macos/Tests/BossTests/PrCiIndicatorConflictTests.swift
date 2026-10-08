import XCTest
@testable import Boss

/// The CI marker reflects CI only; a merge conflict is a separate
/// `PrConflictIndicator`. A conflicting PR must still never look ready.
final class PrCiIndicatorConflictTests: XCTestCase {

    func testCISuccessWithConflictingShowsGreenCIPlusConflictIndicator() {
        let ci = PrCiIndicator(state: "success")
        XCTAssertEqual(ci.systemImage, "checkmark.circle.fill")
        XCTAssertEqual(ci.tint, .green)
        let conflict = PrConflictIndicator(prMergeableState: "conflicting")
        XCTAssertTrue(conflict.isVisible)
        XCTAssertEqual(conflict.tooltipText, "PR has merge conflicts")
    }

    func testCIFailureWithConflictingShowsRedCIPlusConflictIndicator() {
        let ci = PrCiIndicator(state: "fail")
        XCTAssertEqual(ci.systemImage, "xmark.circle.fill")
        XCTAssertEqual(ci.tint, .red)
        XCTAssertTrue(PrConflictIndicator(prMergeableState: "conflicting").isVisible)
    }

    func testCISuccessWithMergeableShowsGreenCIAndNoConflictIndicator() {
        let ci = PrCiIndicator(state: "success")
        XCTAssertEqual(ci.tint, .green)
        XCTAssertFalse(PrConflictIndicator(prMergeableState: "mergeable").isVisible)
        XCTAssertFalse(PrConflictIndicator(prMergeableState: nil).isVisible)
    }

    func testPendingCIStaysYellow() {
        let ci = PrCiIndicator(state: "in_progress")
        XCTAssertEqual(ci.systemImage, "clock.fill")
        XCTAssertEqual(ci.tint, .yellow)
    }

    // MARK: - Shared predicate

    /// `PrMergeability` is the single spelling of the conflicting state,
    /// shared with `MergeQueueBadge` and `PrConflictIndicator`. Only the exact wire value counts —
    /// other mergeability values must not be swept into "conflicting", or
    /// the badge would go red on PRs that merge fine.
    func testOnlyConflictingWireValueCountsAsConflict() {
        XCTAssertTrue(PrMergeability.isConflicting("conflicting"))
        XCTAssertFalse(PrMergeability.isConflicting(nil))
        for benign in ["mergeable", "unknown", "unstable", "blocked", "clean", "draft", ""] {
            XCTAssertFalse(
                PrMergeability.isConflicting(benign),
                "\(benign) must not be treated as a merge conflict"
            )
        }
    }
}
