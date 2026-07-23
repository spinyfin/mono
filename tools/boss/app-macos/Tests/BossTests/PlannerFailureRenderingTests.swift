import XCTest
@testable import Boss

/// Regression coverage for the Planner error banner rendering: a
/// `planner_failed` run's `result_summary` is an engine-formatted string
/// (`populator.rs`: `"planner {tag}: {detail}"`) that can be an arbitrarily
/// long, multiply-escaped serde diagnostic (see `Models+Planner.swift`).
/// These tests cover the pure derivations the rendering leans on —
/// `plannerFailureHeadline` and `String.unescapedForDisplay` — independent
/// of the SwiftUI views that consume them.
final class PlannerFailureRenderingTests: XCTestCase {

    func testHeadlineForInvalidOutputTag() {
        let run = makeRun(outcome: "planner_failed", resultSummary: "planner invalid_output: tool input did not match the PlannerOutput schema")
        XCTAssertEqual(
            run.plannerFailureHeadline,
            "The planner returned output that did not match the expected schema."
        )
    }

    func testHeadlineForKnownTags() {
        XCTAssertEqual(
            makeRun(outcome: "planner_failed", resultSummary: "planner no_api_key: ANTHROPIC_API_KEY not configured").plannerFailureHeadline,
            "The planner call failed: no model API key is configured on the engine."
        )
        XCTAssertEqual(
            makeRun(outcome: "planner_failed", resultSummary: "planner api_error: anthropic returned 429: rate limited").plannerFailureHeadline,
            "The planner call failed: the model API returned an error."
        )
        XCTAssertEqual(
            makeRun(outcome: "planner_failed", resultSummary: "planner transport_error: connection reset").plannerFailureHeadline,
            "The planner call failed: the request to the model could not complete."
        )
    }

    func testHeadlineFallsBackForUnrecognizedTag() {
        // A future engine-side tag the app doesn't know about yet must still
        // render a sensible headline instead of nil/blank.
        let run = makeRun(outcome: "planner_failed", resultSummary: "planner some_new_tag: detail text")
        XCTAssertEqual(run.plannerFailureHeadline, "The planner call failed unexpectedly.")
    }

    func testHeadlineIsNilForNonFailureOutcomes() {
        let run = makeRun(outcome: "staged", resultSummary: "created 5 tasks, 3 edges")
        XCTAssertNil(run.plannerFailureHeadline)
    }

    func testHeadlineIsNilWhenResultSummaryMissing() {
        let run = makeRun(outcome: "planner_failed", resultSummary: nil)
        XCTAssertNil(run.plannerFailureHeadline)
    }

    func testHeadlineIsNilWhenResultSummaryLacksThePlannerPrefix() {
        // A malformed/unparseable result_summary (no "planner <tag>: " prefix
        // at all) must fall back to nil, not the generic sentence — that
        // fallback is reserved for a well-formed but unrecognized tag.
        let run = makeRun(outcome: "planner_failed", resultSummary: "something went wrong")
        XCTAssertNil(run.plannerFailureHeadline)
    }

    func testUnescapedForDisplayCollapsesNestedQuoteEscaping() {
        // Mirrors the reported bug: a serde error message embedding the
        // `Debug` form of a `Vec<String>`, itself carrying `Debug`-escaped
        // quotes from the original `[effort-classification]` values — three
        // levels of `\"` nesting around "single-surface" that should all
        // collapse to plain, readable quote characters.
        let raw = #"invalid type: string "[\"[effort-classification] reasons=\\\"single-surface\\\"\"]", expected struct PlannerOutput"#
        let expected = #"invalid type: string "["[effort-classification] reasons="single-surface""]", expected struct PlannerOutput"#
        XCTAssertEqual(raw.unescapedForDisplay, expected)
    }

    func testUnescapedForDisplayConvertsEscapedNewlinesAndTabs() {
        XCTAssertEqual("line one\\nline two\\tindented".unescapedForDisplay, "line one\nline two\tindented")
    }

    func testUnescapedForDisplayIsIdempotentOnPlainText() {
        let plain = "anthropic returned 429: rate limited"
        XCTAssertEqual(plain.unescapedForDisplay, plain)
    }

    func testDisclosureNeededForManyShortLinesUnderCharacterThreshold() {
        // Eight 20-character lines is only 167 characters (well under the
        // 200-char threshold) but exceeds the 4-line collapsed cap, so the
        // "Show more" affordance must still appear — this is exactly the
        // "truncated with no way to expand" case the disclosure exists to
        // prevent.
        let manyShortLines = Array(repeating: "01234567890123456789", count: 8).joined(separator: "\n")
        XCTAssertLessThanOrEqual(manyShortLines.count, PlannerResultSummaryLayout.disclosureCharacterThreshold)
        XCTAssertTrue(PlannerResultSummaryLayout.needsDisclosure(for: manyShortLines))
    }

    func testDisclosureNotNeededForShortSingleLineText() {
        XCTAssertFalse(PlannerResultSummaryLayout.needsDisclosure(for: "created 5 tasks, 3 edges"))
    }

    // MARK: - Helpers

    private func makeRun(outcome: String, resultSummary: String?) -> PlannerRun {
        PlannerRun(
            id: "run_1",
            projectID: "proj_1",
            productID: "prod_1",
            designTaskID: nil,
            caller: "operator",
            docRef: nil,
            model: nil,
            inputSummary: nil,
            rawOutput: nil,
            effortAudit: nil,
            notes: nil,
            outcome: outcome,
            resultSummary: resultSummary,
            createdAt: "2026-07-22T00:00:00Z",
            updatedAt: "2026-07-22T00:00:00Z"
        )
    }
}
