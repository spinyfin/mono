import XCTest
@testable import Boss

/// Regression coverage for the "kanban chore card shows stale Merge
/// Conflict badge after engine cleared the conflict" report. The
/// engine's invariant is: the scalar `Task::blocked_reason` is `NULL`
/// whenever `status != 'blocked'`. The macOS reducer must stay
/// defensive against a transient state where it has applied the new
/// status (chore is in `Review`) but is still carrying the stale
/// `blockedReason` because an earlier `work_invalidated` envelope was
/// dropped on `events.sock` or the work-tree refresh is still in
/// flight. The badge derivation rule that backs the kanban card is
/// captured by [[WorkBlockedBadge.badgeText(for:)]]; these tests pin
/// the rule down.
final class WorkBlockedBadgeTests: XCTestCase {

    // MARK: badgeText — projection-of-status rule

    /// In-review chore with a stale `blockedReason == "merge_conflict"`:
    /// the badge must NOT render. This is the regression case.
    func testInReviewChoreWithStaleBlockedReasonHidesBadge() {
        let task = makeTask(status: "in_review", blockedReason: "merge_conflict")
        XCTAssertNil(WorkBlockedBadge.badgeText(for: task))
    }

    /// Same desync on a `todo` chore — should also hide the badge.
    func testTodoChoreWithStaleBlockedReasonHidesBadge() {
        let task = makeTask(status: "todo", blockedReason: "merge_conflict")
        XCTAssertNil(WorkBlockedBadge.badgeText(for: task))
    }

    /// `done` rows similarly never show a blocked badge even with a
    /// lingering reason value.
    func testDoneChoreWithStaleBlockedReasonHidesBadge() {
        let task = makeTask(status: "done", blockedReason: "merge_conflict")
        XCTAssertNil(WorkBlockedBadge.badgeText(for: task))
    }

    /// `active` (Doing lane) with stale reason — still no badge.
    func testActiveChoreWithStaleBlockedReasonHidesBadge() {
        let task = makeTask(status: "active", blockedReason: "ci_failure")
        XCTAssertNil(WorkBlockedBadge.badgeText(for: task))
    }

    // MARK: badgeText — blocked rows show the specific reason

    func testBlockedMergeConflictShowsMergeConflictLabel() {
        let task = makeTask(status: "blocked", blockedReason: "merge_conflict")
        XCTAssertEqual(WorkBlockedBadge.badgeText(for: task), "Merge Conflict")
    }

    func testBlockedDependencyShowsDependencyLabel() {
        let task = makeTask(status: "blocked", blockedReason: "dependency")
        XCTAssertEqual(WorkBlockedBadge.badgeText(for: task), "Dependency")
    }

    func testBlockedCIFailureShowsCIFailureLabel() {
        let task = makeTask(status: "blocked", blockedReason: "ci_failure")
        XCTAssertEqual(WorkBlockedBadge.badgeText(for: task), "CI Failure")
    }

    func testBlockedCIFailureExhaustedShowsCIFailedLabel() {
        let task = makeTask(status: "blocked", blockedReason: "ci_failure_exhausted")
        XCTAssertEqual(WorkBlockedBadge.badgeText(for: task), "CI Failed")
    }

    func testBlockedReviewFeedbackShowsReviewLabel() {
        let task = makeTask(status: "blocked", blockedReason: "review_feedback")
        XCTAssertEqual(WorkBlockedBadge.badgeText(for: task), "Review")
    }

    /// Legacy blocked row that predates the `blocked_reason` column —
    /// the field is `nil` on the wire. The badge falls back to the
    /// generic "Blocked" tag so the card still flags the state.
    func testBlockedWithoutReasonShowsGenericBlockedLabel() {
        let task = makeTask(status: "blocked", blockedReason: nil)
        XCTAssertEqual(WorkBlockedBadge.badgeText(for: task), "Blocked")
    }

    /// Unknown / future `blocked_reason` codes degrade to a title-cased
    /// version of the raw string rather than vanishing.
    func testBlockedWithUnknownReasonFallsBackToTitleCase() {
        let task = makeTask(status: "blocked", blockedReason: "mystery_signal")
        XCTAssertEqual(WorkBlockedBadge.badgeText(for: task), "Mystery Signal")
    }

    // MARK: label(forReason:) — vocabulary-only checks

    func testReasonLabelVocabulary() {
        XCTAssertEqual(WorkBlockedBadge.label(forReason: "merge_conflict"), "Merge Conflict")
        XCTAssertEqual(WorkBlockedBadge.label(forReason: "dependency"), "Dependency")
        XCTAssertEqual(WorkBlockedBadge.label(forReason: "ci_failure"), "CI Failure")
        XCTAssertEqual(WorkBlockedBadge.label(forReason: "ci_failure_exhausted"), "CI Failed")
        XCTAssertEqual(WorkBlockedBadge.label(forReason: "review_feedback"), "Review")
    }

    // MARK: badgeTooltip — verbatim detail, falling back to raw reason

    /// `blockedDetail` set: the tooltip shows it verbatim — no
    /// title-casing, no truncation, identifiers preserved exactly.
    func testBadgeTooltipShowsVerbatimDetailWhenSet() {
        let task = makeTask(
            status: "blocked",
            blockedReason: "FUTURE",
            blockedDetail: "deferred scope per design doc; requires explicit operator approval (pr_created)"
        )
        XCTAssertEqual(
            WorkBlockedBadge.badgeTooltip(for: task),
            "deferred scope per design doc; requires explicit operator approval (pr_created)"
        )
    }

    /// No `blockedDetail`: the tooltip falls back to the raw (untitled,
    /// untruncated) `blockedReason` rather than the transformed label —
    /// this is what makes existing rows with prose already in the label
    /// field readable via hover.
    func testBadgeTooltipFallsBackToRawReasonWhenNoDetail() {
        let task = makeTask(status: "blocked", blockedReason: "mystery_signal", blockedDetail: nil)
        XCTAssertEqual(WorkBlockedBadge.badgeTooltip(for: task), "mystery_signal")
    }

    /// Non-blocked status: no tooltip, mirroring `badgeText`'s status gate.
    func testBadgeTooltipHiddenWhenNotBlocked() {
        let task = makeTask(status: "in_review", blockedReason: "merge_conflict", blockedDetail: "some detail")
        XCTAssertNil(WorkBlockedBadge.badgeTooltip(for: task))
    }

    /// Neither reason nor detail: no tooltip.
    func testBadgeTooltipNilWithNoReasonOrDetail() {
        let task = makeTask(status: "blocked", blockedReason: nil, blockedDetail: nil)
        XCTAssertNil(WorkBlockedBadge.badgeTooltip(for: task))
    }

    // MARK: hasMoreInfo — affordance dot gating

    /// A known short discriminator with no detail: nothing more to show,
    /// so no affordance dot.
    func testHasMoreInfoFalseForKnownReasonWithoutDetail() {
        let task = makeTask(status: "blocked", blockedReason: "dependency", blockedDetail: nil)
        XCTAssertFalse(WorkBlockedBadge.hasMoreInfo(for: task))
    }

    /// A custom/freeform reason that hit the title-casing fallback: the
    /// raw string is exactly what the tooltip fallback recovers, so this
    /// counts as "more info" even without an explicit detail.
    func testHasMoreInfoTrueForCustomReasonWithoutDetail() {
        let task = makeTask(status: "blocked", blockedReason: "mystery_signal", blockedDetail: nil)
        XCTAssertTrue(WorkBlockedBadge.hasMoreInfo(for: task))
    }

    /// Any reason with an explicit detail set: always "more info",
    /// regardless of whether the reason itself is a known discriminator.
    func testHasMoreInfoTrueWhenDetailSet() {
        let task = makeTask(status: "blocked", blockedReason: "dependency", blockedDetail: "waiting on T42")
        XCTAssertTrue(WorkBlockedBadge.hasMoreInfo(for: task))
    }

    /// No reason at all (legacy blocked row): no detail possible, no dot.
    func testHasMoreInfoFalseWithNoReason() {
        let task = makeTask(status: "blocked", blockedReason: nil, blockedDetail: nil)
        XCTAssertFalse(WorkBlockedBadge.hasMoreInfo(for: task))
    }

    /// Non-blocked status never shows the dot, mirroring the other gates.
    func testHasMoreInfoFalseWhenNotBlocked() {
        let task = makeTask(status: "todo", blockedReason: "mystery_signal", blockedDetail: "detail")
        XCTAssertFalse(WorkBlockedBadge.hasMoreInfo(for: task))
    }

    // MARK: - Helpers

    private func makeTask(
        status: String,
        blockedReason: String?,
        blockedDetail: String? = nil,
        id: String = "task_\(UUID().uuidString)"
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Test chore",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/x/y/pull/42",
            deletedAt: nil,
            createdAt: "2026-05-14T00:00:00Z",
            updatedAt: "2026-05-14T00:00:00Z",
            blockedReason: blockedReason,
            blockedDetail: blockedDetail
        )
    }
}
