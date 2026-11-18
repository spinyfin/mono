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

    func testDeferredConflictIsVisibleAndInformational() {
        let indicator = PrConflictIndicator(prMergeableState: "conflicting", resolutionDeferred: true)
        XCTAssertTrue(indicator.isVisible)
        XCTAssertEqual(indicator.labelText, "Conflicts — resolves on merge")
        XCTAssertEqual(indicator.tint, .secondary)
        XCTAssertEqual(indicator.systemImage, "info.circle")
        XCTAssertTrue(indicator.tooltipText.contains("automatically when you request a merge"))
    }

    func testReleasedOrFailedResolutionKeepsErrorPresentation() {
        let indicator = PrConflictIndicator(prMergeableState: "conflicting", resolutionDeferred: false)
        XCTAssertTrue(indicator.isVisible)
        XCTAssertEqual(indicator.labelText, "conflicts")
        XCTAssertEqual(indicator.tint, .red)
        XCTAssertEqual(indicator.systemImage, "exclamationmark.triangle.fill")
    }

    func testDeferralDoesNotInventAConflict() {
        XCTAssertFalse(PrConflictIndicator(prMergeableState: "mergeable", resolutionDeferred: true).isVisible)
        XCTAssertFalse(PrConflictIndicator(prMergeableState: nil, resolutionDeferred: true).isVisible)
    }

    @MainActor
    func testEngineDeferralPayloadReachesFooterAndDefaultsToError() throws {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var payload: [String: Any] = [
            "id": "conflict-card", "product_id": "product", "kind": "chore",
            "name": "Conflict card", "description": "", "status": "in_review",
            "created_at": "2026-11-14", "updated_at": "2026-11-14",
            "pr_mergeable_state": "conflicting",
            "pr_url": "https://github.com/x/y/pull/42",
        ]
        for deferred in [nil, true, false] as [Bool?] {
            payload["conflict_resolution_deferred"] = deferred
            let task = try XCTUnwrap(client.parseTask(payload))
            let snapshot = WorkCardSnapshot.build(task: task, context: WorkCardSnapshotContext(column: .review))
            let footer = WorkBoardCardFooterSlice(snapshot: snapshot)
            XCTAssertEqual(footer.conflictResolutionDeferred, deferred ?? false)
            XCTAssertEqual(footer.prMergeableState, "conflicting")
        }
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
