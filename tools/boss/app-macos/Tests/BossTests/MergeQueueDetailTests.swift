import XCTest
@testable import Boss

/// Pins the `merge_queue_detail` JSON parsing contract (`{"position",
/// "state", "enqueued_at", "section_order"}`) the merge poller writes while
/// a PR sits in GitHub's merge queue or has Merge When Ready armed, and the
/// display-state vocabulary the Merging-section badge renders from it. Kept
/// free of SwiftUI (mirrors `AutomationTimeTests`).
final class MergeQueueDetailTests: XCTestCase {

    func testParsesFullBlob() {
        let json = #"""
        {"position":1,"state":"AWAITING_CHECKS","enqueued_at":"2026-07-10T11:54:54Z","section_order":1}
        """#
        let detail = MergeQueueDetail.parse(json)
        XCTAssertEqual(detail?.position, 1)
        XCTAssertEqual(detail?.state, "AWAITING_CHECKS")
        XCTAssertEqual(detail?.enqueuedAt, "2026-07-10T11:54:54Z")
        XCTAssertEqual(detail?.sectionOrder, 1)
    }

    func testParsesPartialBlobWithMissingFields() {
        let json = #"{"position":null,"state":"QUEUED","enqueued_at":null,"section_order":2}"#
        let detail = MergeQueueDetail.parse(json)
        XCTAssertNil(detail?.position)
        XCTAssertEqual(detail?.state, "QUEUED")
        XCTAssertNil(detail?.enqueuedAt)
        XCTAssertEqual(detail?.sectionOrder, 2)
    }

    func testParsesMergeWhenReadyBlobWithNoQueueFields() {
        // Merge-when-ready-armed-but-not-queued shape: no position/state/
        // enqueued_at, just a section_order derived from `enabledAt`.
        let json = #"{"position":null,"state":null,"enqueued_at":null,"section_order":1783684494}"#
        let detail = MergeQueueDetail.parse(json)
        XCTAssertNil(detail?.position)
        XCTAssertNil(detail?.state)
        XCTAssertNil(detail?.enqueuedAt)
        XCTAssertEqual(detail?.sectionOrder, 1_783_684_494)
    }

    func testParseReturnsNilForNilEmptyOrGarbageInput() {
        XCTAssertNil(MergeQueueDetail.parse(nil))
        XCTAssertNil(MergeQueueDetail.parse(""))
        XCTAssertNil(MergeQueueDetail.parse("not-json"))
        XCTAssertNil(MergeQueueDetail.parse("[]"))
    }

    func testDisplayStateMapsKnownGitHubEnumValues() {
        XCTAssertEqual(MergeQueueDetail(state: "AWAITING_CHECKS").displayState, "awaiting checks")
        XCTAssertEqual(MergeQueueDetail(state: "MERGEABLE").displayState, "mergeable")
        XCTAssertEqual(MergeQueueDetail(state: "LOCKED").displayState, "locked")
        XCTAssertEqual(MergeQueueDetail(state: "QUEUED").displayState, "queued")
        XCTAssertEqual(MergeQueueDetail(state: "UNMERGEABLE").displayState, "unmergeable")
    }

    func testDisplayStateFallsBackForUnrecognisedValue() {
        XCTAssertEqual(MergeQueueDetail(state: "SOME_NEW_STATE").displayState, "some new state")
    }

    func testDisplayStateNilForNilOrEmptyState() {
        XCTAssertNil(MergeQueueDetail(state: nil).displayState)
        XCTAssertNil(MergeQueueDetail(state: "").displayState)
    }

    // MARK: - source / queue_state (Trunk queue poller extension)

    func testParsesTrunkSourceAndQueueStateFields() {
        let json = #"""
        {"source":"trunk","state":"testing","position":3,"enqueued_at":"2026-07-20T09:00:00Z","queue_state":"RUNNING","section_order":3}
        """#
        let detail = MergeQueueDetail.parse(json)
        XCTAssertEqual(detail?.source, "trunk")
        XCTAssertEqual(detail?.queueState, "RUNNING")
        XCTAssertTrue(detail?.isTrunk ?? false)
    }

    func testGitHubBlobHasNilSourceAndQueueState() {
        let json = #"""
        {"position":1,"state":"AWAITING_CHECKS","enqueued_at":"2026-07-10T11:54:54Z","section_order":1}
        """#
        let detail = MergeQueueDetail.parse(json)
        XCTAssertNil(detail?.source)
        XCTAssertNil(detail?.queueState)
        XCTAssertFalse(detail?.isTrunk ?? true)
    }

    // MARK: - trunkBadgeText

    func testTrunkBadgeTextPendingShowsQueuePosition() {
        XCTAssertEqual(MergeQueueDetail(position: 3, state: "pending", source: "trunk").trunkBadgeText, "#3")
    }

    func testTrunkBadgeTextPendingWithNoPositionFallsBackToQueued() {
        XCTAssertEqual(MergeQueueDetail(state: "pending", source: "trunk").trunkBadgeText, "Queued")
    }

    func testTrunkBadgeTextTesting() {
        XCTAssertEqual(MergeQueueDetail(state: "testing", source: "trunk").trunkBadgeText, "Testing")
    }

    func testTrunkBadgeTextTestsPassed() {
        XCTAssertEqual(MergeQueueDetail(state: "tests_passed", source: "trunk").trunkBadgeText, "Merging…")
    }

    func testTrunkBadgeTextNotReady() {
        XCTAssertEqual(MergeQueueDetail(state: "not_ready", source: "trunk").trunkBadgeText, "Waiting on readiness")
    }

    func testTrunkBadgeTextNilForFailedAndPendingFailure() {
        XCTAssertNil(MergeQueueDetail(state: "failed", source: "trunk").trunkBadgeText)
        XCTAssertNil(MergeQueueDetail(state: "pending_failure", source: "trunk").trunkBadgeText)
    }

    func testTrunkBadgeTextNilForCancelledAndMerged() {
        XCTAssertNil(MergeQueueDetail(state: "cancelled", source: "trunk").trunkBadgeText)
        XCTAssertNil(MergeQueueDetail(state: "merged", source: "trunk").trunkBadgeText)
    }

    func testTrunkBadgeTextRendersRawStringForUnknownState() {
        XCTAssertEqual(MergeQueueDetail(state: "some_future_state", source: "trunk").trunkBadgeText, "some_future_state")
    }

    func testTrunkBadgeTextNilForNonTrunkEntry() {
        XCTAssertNil(MergeQueueDetail(state: "testing").trunkBadgeText)
    }

    // MARK: - isTrunkTerminal

    func testIsTrunkTerminalForFailedAndPendingFailure() {
        XCTAssertTrue(MergeQueueDetail(state: "failed", source: "trunk").isTrunkTerminal)
        XCTAssertTrue(MergeQueueDetail(state: "pending_failure", source: "trunk").isTrunkTerminal)
        XCTAssertFalse(MergeQueueDetail(state: "testing", source: "trunk").isTrunkTerminal)
        XCTAssertFalse(MergeQueueDetail(state: "failed").isTrunkTerminal)
    }

    func testIsTrunkTerminalForCancelledAndMerged() {
        XCTAssertTrue(MergeQueueDetail(state: "cancelled", source: "trunk").isTrunkTerminal)
        XCTAssertTrue(MergeQueueDetail(state: "merged", source: "trunk").isTrunkTerminal)
        XCTAssertFalse(MergeQueueDetail(state: "cancelled").isTrunkTerminal)
    }

    // MARK: - queueStateBanner

    func testQueueStateBannerNilWhenRunning() {
        XCTAssertNil(MergeQueueDetail(source: "trunk", queueState: "RUNNING").queueStateBanner)
    }

    func testQueueStateBannerNilWhenAbsent() {
        XCTAssertNil(MergeQueueDetail(source: "trunk").queueStateBanner)
    }

    func testQueueStateBannerNilForNonTrunkEntry() {
        XCTAssertNil(MergeQueueDetail(queueState: "PAUSED").queueStateBanner)
    }

    func testQueueStateBannerForPausedAndDraining() {
        XCTAssertEqual(MergeQueueDetail(source: "trunk", queueState: "PAUSED").queueStateBanner, "Trunk queue paused")
        XCTAssertEqual(MergeQueueDetail(source: "trunk", queueState: "DRAINING").queueStateBanner, "Trunk queue draining")
    }

    func testQueueStateBannerFallsBackForUnrecognisedValue() {
        XCTAssertEqual(
            MergeQueueDetail(source: "trunk", queueState: "SOME_NEW_STATE").queueStateBanner,
            "Trunk queue some_new_state"
        )
    }
}
