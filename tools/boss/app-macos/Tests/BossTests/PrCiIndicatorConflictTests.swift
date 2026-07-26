import XCTest
@testable import Boss

/// A Review-lane card whose PR is `pr_mergeable_state == "conflicting"` must
/// never render as CI-green, even when `mergeQueueState` is `nil` (i.e. not
/// merge-queued / no auto-merge armed, so `WorkTask.isInMergingSection` is
/// false and `MergeQueueBadge` never gets a chance to run its own conflict
/// check). This pins the mono#2366 fix: `PrCiIndicator` — the badge actually
/// rendered on such a card — must apply the same "conflict pre-empts CI"
/// rule `MergeQueueBadge` already applied for the Merging section.
final class PrCiIndicatorConflictTests: XCTestCase {

    func testConflictingMergeableStatePreemptsSuccessfulCI() {
        let indicator = PrCiIndicator(state: "success", detail: nil, prMergeableState: "conflicting")
        XCTAssertTrue(indicator.isConflicting)
        XCTAssertEqual(indicator.systemImage, "xmark.circle.fill")
        XCTAssertNotEqual(indicator.tint, .green, "a conflicting PR must never render the green success icon")
        XCTAssertEqual(indicator.tint, .red)
        XCTAssertEqual(indicator.tooltipText, "PR has merge conflicts")
    }

    func testConflictingMergeableStatePreemptsInProgressCI() {
        let indicator = PrCiIndicator(state: "in_progress", detail: nil, prMergeableState: "conflicting")
        XCTAssertEqual(indicator.systemImage, "xmark.circle.fill")
        XCTAssertEqual(indicator.tint, .red)
    }

    func testNonConflictingMergeableStateLeavesCIStateUnaffected() {
        let indicator = PrCiIndicator(state: "success", detail: nil, prMergeableState: "mergeable")
        XCTAssertFalse(indicator.isConflicting)
        XCTAssertEqual(indicator.systemImage, "checkmark.circle.fill")
        XCTAssertEqual(indicator.tint, .green)
    }

    func testNilMergeableStateLeavesCIStateUnaffected() {
        // Merging-section cards (or a not-yet-polled state) pass nil here;
        // must fall through to the plain CI-state rendering.
        let indicator = PrCiIndicator(state: "success", detail: nil, prMergeableState: nil)
        XCTAssertFalse(indicator.isConflicting)
        XCTAssertEqual(indicator.systemImage, "checkmark.circle.fill")
        XCTAssertEqual(indicator.tint, .green)
    }

    func testFailingCIWithoutConflictStillRendersFailState() {
        let indicator = PrCiIndicator(state: "fail", detail: nil, prMergeableState: "mergeable")
        XCTAssertFalse(indicator.isConflicting)
        XCTAssertEqual(indicator.systemImage, "xmark.circle.fill")
        XCTAssertEqual(indicator.tint, .red)
    }

    /// The default (no `prMergeableState` argument) must behave like `nil`,
    /// so call sites that never learned about mergeability are unaffected.
    func testDefaultMergeableStateLeavesCIStateUnaffected() {
        let indicator = PrCiIndicator(state: "success")
        XCTAssertFalse(indicator.isConflicting)
        XCTAssertEqual(indicator.systemImage, "checkmark.circle.fill")
        XCTAssertEqual(indicator.tint, .green)
    }

    // MARK: - Shared predicate

    /// `PrMergeability` is the single spelling of the conflicting state,
    /// shared with `MergeQueueBadge`. Only the exact wire value counts —
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

    /// The indicator must agree with the shared predicate rather than
    /// carrying its own comparison.
    func testIndicatorDelegatesToSharedPredicate() {
        for raw in ["conflicting", "mergeable", "unknown", nil] {
            let indicator = PrCiIndicator(state: "success", detail: nil, prMergeableState: raw)
            XCTAssertEqual(indicator.isConflicting, PrMergeability.isConflicting(raw))
        }
    }
}
