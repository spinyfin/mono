import XCTest
@testable import Boss

/// Unit coverage for free-form work-item tag presentation on kanban cards:
/// empty collapse, per-tag truncation, and overflow `+N`.
final class WorkTagPresentationTests: XCTestCase {

    func testEmptyTagsCollapseToNoChips() {
        let result = WorkTagPresentation.chips(for: [])
        XCTAssertTrue(result.labels.isEmpty)
        XCTAssertNil(result.overflow)
    }

    func testWhitespaceOnlyTagsCollapse() {
        let result = WorkTagPresentation.chips(for: ["  ", "\t", ""])
        XCTAssertTrue(result.labels.isEmpty)
        XCTAssertNil(result.overflow)
    }

    func testShortTagsPassThrough() {
        let result = WorkTagPresentation.chips(for: ["needs-human", "ci-flake"])
        XCTAssertEqual(result.labels, ["needs-human", "ci-flake"])
        XCTAssertNil(result.overflow)
    }

    func testLongTagTruncatesWithEllipsis() {
        let long = String(repeating: "x", count: WorkTagPresentation.maxTagLength + 5)
        let label = WorkTagPresentation.displayLabel(for: long)
        XCTAssertNotNil(label)
        // Truncated to maxTagLength-1 chars + ellipsis → maxTagLength total glyphs
        // of the displayed string (excluding the ellipsis character itself would
        // be max-1; with ellipsis the string length is maxTagLength).
        XCTAssertEqual(label!.count, WorkTagPresentation.maxTagLength)
        XCTAssertTrue(label!.hasSuffix("…"))
        XCTAssertEqual(
            String(label!.dropLast()),
            String(repeating: "x", count: WorkTagPresentation.maxTagLength - 1)
        )
    }

    func testTagAtMaxLengthIsNotTruncated() {
        let exact = String(repeating: "a", count: WorkTagPresentation.maxTagLength)
        XCTAssertEqual(WorkTagPresentation.displayLabel(for: exact), exact)
    }

    func testOverflowProducesPlusN() {
        // Drive overflow by exceeding maxVisibleChips with unique labels.
        var tags: [String] = []
        for i in 0..<(WorkTagPresentation.maxVisibleChips + 2) {
            tags.append("t\(i)")
        }
        let result = WorkTagPresentation.chips(for: tags)
        XCTAssertEqual(result.labels.count, WorkTagPresentation.maxVisibleChips)
        XCTAssertEqual(result.overflow, 2)
        XCTAssertEqual(result.labels.first, "t0")
    }

    func testTrimsSurroundingWhitespace() {
        XCTAssertEqual(
            WorkTagPresentation.displayLabel(for: "  codex  "),
            "codex"
        )
    }
}
