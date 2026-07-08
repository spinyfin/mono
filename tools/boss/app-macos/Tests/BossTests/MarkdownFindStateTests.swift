import XCTest
@testable import Boss

/// Tests for the ⌘F find-in-document feature: the pure substring matcher
/// (`MarkdownSearch`), the find-state navigation/counter logic
/// (`MarkdownFindState`), and the focus-contention guard against T548's
/// type-to-comment trigger (`CommentLayer.suppressTypeToComment`).
@MainActor
final class MarkdownFindStateTests: XCTestCase {

    // MARK: - MarkdownSearch

    func testFindMatchesIsCaseInsensitive() {
        let matches = MarkdownSearch.findMatches(of: "hello", in: "Hello world, HELLO again")
        XCTAssertEqual(matches, [0..<5, 13..<18])
    }

    func testFindMatchesNonOverlapping() {
        let matches = MarkdownSearch.findMatches(of: "aa", in: "aaaa")
        // "aa" at 0..<2, then search resumes at 2, finds "aa" at 2..<4 — not the
        // overlapping match at 1..<3.
        XCTAssertEqual(matches, [0..<2, 2..<4])
    }

    func testFindMatchesEmptyQueryReturnsNoMatches() {
        XCTAssertEqual(MarkdownSearch.findMatches(of: "", in: "some text"), [])
    }

    func testFindMatchesNoOccurrenceReturnsEmpty() {
        XCTAssertEqual(MarkdownSearch.findMatches(of: "xyz", in: "some text"), [])
    }

    // MARK: - MarkdownFindState

    func testUpdateSourceComputesMatchesAgainstRenderedPlainText() {
        let state = MarkdownFindState()
        state.query = "external_ref"
        // "external_ref" appears once in prose and once inside an inline-code
        // span — the plain-text projection strips the backticks, so both are
        // findable by the same literal query, matching visible text rather
        // than markdown syntax.
        state.updateSource("The token `external_ref` is used in external_ref lookups.", baseURL: nil)
        XCTAssertEqual(state.matches.count, 2)
        XCTAssertEqual(state.currentIndex, 0)
        XCTAssertEqual(state.counterText, "1 of 2")
    }

    func testEmptyQueryHasNoCounterText() {
        let state = MarkdownFindState()
        state.updateSource("hello world", baseURL: nil)
        XCTAssertEqual(state.query, "")
        XCTAssertEqual(state.counterText, "")
    }

    func testNonEmptyQueryWithZeroMatchesReportsNotFound() {
        let state = MarkdownFindState()
        state.updateSource("hello world", baseURL: nil)
        state.query = "zzz"
        XCTAssertTrue(state.matches.isEmpty)
        XCTAssertNil(state.currentIndex)
        XCTAssertEqual(state.counterText, "Not found")
    }

    func testSelectNextWrapsAtEnd() {
        let state = MarkdownFindState()
        state.updateSource("cat cat cat", baseURL: nil)
        state.query = "cat"
        XCTAssertEqual(state.matches.count, 3)
        XCTAssertEqual(state.currentIndex, 0)
        state.selectNext()
        XCTAssertEqual(state.currentIndex, 1)
        state.selectNext()
        XCTAssertEqual(state.currentIndex, 2)
        state.selectNext()
        XCTAssertEqual(state.currentIndex, 0, "should wrap back to the first match")
    }

    func testSelectPreviousWrapsAtStart() {
        let state = MarkdownFindState()
        state.updateSource("cat cat cat", baseURL: nil)
        state.query = "cat"
        XCTAssertEqual(state.currentIndex, 0)
        state.selectPrevious()
        XCTAssertEqual(state.currentIndex, 2, "should wrap back to the last match")
    }

    func testNavigationNonceAdvancesOnQueryChangeEvenWhenIndexStaysZero() {
        let state = MarkdownFindState()
        state.updateSource("apple banana apple", baseURL: nil)
        state.query = "apple"
        let firstNonce = state.navigationNonce
        state.query = "banana"
        // currentIndex is 0 for both queries, but the nonce must still
        // advance so the viewer knows to re-highlight and re-reveal.
        XCTAssertEqual(state.currentIndex, 0)
        XCTAssertGreaterThan(state.navigationNonce, firstNonce)
    }

    func testCloseDoesNotClearQueryOrMatches() {
        let state = MarkdownFindState()
        state.updateSource("hello world", baseURL: nil)
        state.query = "hello"
        state.open()
        state.close()
        XCTAssertFalse(state.isActive)
        XCTAssertEqual(state.query, "hello", "reopening with ⌘F should restore the last query")
        XCTAssertEqual(state.matches.count, 1)
    }

    // MARK: - Focus-contention guard (T548 collision)

    func testSuppressTypeToCommentShortCircuitsKeyEventConsumption() {
        let layer = CommentLayer()
        layer.suppressTypeToComment = true
        // Even a plain-letter keydown that would otherwise be eligible for
        // type-to-comment must be ignored while the find bar holds focus.
        XCTAssertFalse(layer.shouldConsumeKeyEvent(chars: "a", mods: [], window: nil))
    }

    func testSuppressTypeToCommentDefaultsFalse() {
        let layer = CommentLayer()
        XCTAssertFalse(layer.suppressTypeToComment)
    }
}
