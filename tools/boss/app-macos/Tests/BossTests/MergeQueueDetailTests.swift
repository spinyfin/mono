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
}
