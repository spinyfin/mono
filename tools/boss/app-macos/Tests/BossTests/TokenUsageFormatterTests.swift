import XCTest
@testable import Boss

/// Pins every presentation invariant the Token Usage window must honour.
/// The formatter is the only code allowed to turn token fields into text;
/// these tests are the review checklist for "unknown is never zero".
final class TokenUsageFormatterTests: XCTestCase {
    func testUnmeasuredRunIsEmDashNeverZero() {
        let pre = TokenUsageFormatter.runTotal(
            tokens: nil,
            coverage: .unmeasured,
            caveats: [UsageCaveat(code: "pre_capture", detail: nil)],
            inputConvention: .unknown
        )
        XCTAssertEqual(pre.text, "—")
        XCTAssertEqual(pre.annotation, "not instrumented")
        XCTAssertFalse(pre.display.contains("0"))

        let post = TokenUsageFormatter.runTotal(
            tokens: nil,
            coverage: .unmeasured,
            caveats: [UsageCaveat(code: "usage_unavailable", detail: "No provider token usage observed")],
            inputConvention: .net
        )
        XCTAssertEqual(post.text, "—")
        XCTAssertEqual(post.annotation, "No provider token usage observed")
        XCTAssertFalse(post.display.contains("0"))
    }

    func testMeasuredZeroIsTheOnlyPathToDisplayedZero() {
        let zero = TokenUsageFormatter.runTotal(
            tokens: 0,
            coverage: .zero,
            caveats: [],
            inputConvention: .net
        )
        XCTAssertEqual(zero.text, "0")
        XCTAssertNil(zero.annotation)
        XCTAssertTrue(zero.isZero)
    }

    func testPartialRunTotalIsFloor() {
        let figure = TokenUsageFormatter.runTotal(
            tokens: 2_486,
            coverage: .partial,
            caveats: [UsageCaveat(code: "tokens_partial", detail: "cache_creation")],
            inputConvention: .net
        )
        XCTAssertEqual(figure.text, "2,486")
        XCTAssertEqual(figure.annotation, "floor")
    }

    func testClassUnknownOnAMeasuredRunIsPerCell() {
        let input = TokenUsageFormatter.tokenClass(2_486, coverage: .partial)
        let cacheWrite = TokenUsageFormatter.tokenClass(nil, coverage: .partial)
        let cacheRead = TokenUsageFormatter.tokenClass(24_576, coverage: .partial)
        XCTAssertEqual(input.text, "2,486")
        XCTAssertFalse(input.isUnknown)
        XCTAssertEqual(cacheWrite.text, "—")
        XCTAssertEqual(cacheWrite.annotation, "unknown")
        XCTAssertTrue(cacheWrite.isUnknown)
        XCTAssertEqual(cacheRead.text, "24,576")
        XCTAssertNotEqual(cacheWrite.text, "0")
    }

    func testClassUnknownIsDistinctFromWholeNodeUnmeasured() {
        let unmeasuredCell = TokenUsageFormatter.tokenClass(nil, coverage: .unmeasured)
        let unknownClass = TokenUsageFormatter.tokenClass(nil, coverage: .partial)
        XCTAssertEqual(unmeasuredCell.annotation, "not recorded")
        XCTAssertEqual(unknownClass.annotation, "unknown")
        XCTAssertEqual(unmeasuredCell.text, "—")
        XCTAssertEqual(unknownClass.text, "—")
    }

    func testNodeWithNoMeasuredRunsIsEmDashAndReason() {
        var measurement = CostMeasurement.empty
        measurement.runsTotal = 2
        measurement.runsUnmeasured = 2
        measurement.usageUnavailable = ["No provider token usage observed"]
        let figure = TokenUsageFormatter.measurementTotal(measurement)
        XCTAssertEqual(figure.text, "—")
        XCTAssertEqual(figure.annotation, "No provider token usage observed")
        XCTAssertFalse(figure.display.contains("0"))
    }

    func testPartialCoverageMeasurementIsFloor() {
        var measurement = CostMeasurement.empty
        measurement.runsMeasured = 2
        measurement.runsTotal = 2
        measurement.tokensPartial = true
        measurement.totalTokens = 27_000
        measurement.cacheCreationTokens = nil
        measurement.inputTokens = 2_486
        let total = TokenUsageFormatter.measurementTotal(measurement)
        XCTAssertEqual(total.text, "27,000")
        XCTAssertEqual(total.annotation, "floor")
        let cache = TokenUsageFormatter.measurementClass(nil, measurement: measurement)
        XCTAssertEqual(cache.text, "—")
        XCTAssertEqual(cache.annotation, "unknown")
        let input = TokenUsageFormatter.measurementClass(2_486, measurement: measurement)
        XCTAssertEqual(input.text, "2,486")
    }

    func testUsdEstimateNeverZeroOnUnpriceableAndSubscriptionIsUnpriced() {
        let priced = TokenUsageFormatter.usd(estimated: 1.23, partial: false, unpriceableCause: nil)
        XCTAssertEqual(priced?.text, "est. $1.23")

        let partial = TokenUsageFormatter.usd(estimated: 4.0, partial: true, unpriceableCause: nil)
        XCTAssertEqual(partial?.text, "est. $4.00 (partial)")

        let tiny = TokenUsageFormatter.usd(estimated: 0.001, partial: false, unpriceableCause: nil)
        XCTAssertEqual(tiny?.text, "est. <$0.01")

        let grok = TokenUsageFormatter.usd(
            estimated: nil,
            partial: false,
            unpriceableCause: "subscription_unpriced"
        )
        XCTAssertEqual(grok?.text, "—")
        XCTAssertEqual(grok?.annotation, "unpriced (subscription)")
        XCTAssertFalse(grok!.display.contains("$0.00"))

        let agy = TokenUsageFormatter.usd(
            estimated: nil,
            partial: false,
            unpriceableCause: "subscription_unpriced"
        )
        XCTAssertEqual(agy?.annotation, "unpriced (subscription)")

        let hidden = TokenUsageFormatter.usd(
            estimated: nil,
            partial: true,
            unpriceableCause: "unknown_model"
        )
        XCTAssertNil(hidden)
    }

    func testGrossMarkerOnTotal() {
        let figure = TokenUsageFormatter.runTotal(
            tokens: 95_000,
            coverage: .measured,
            caveats: [UsageCaveat(code: "gross_input", detail: nil)],
            inputConvention: .gross
        )
        XCTAssertEqual(figure.annotation, "gross")
    }

    func testCaptureBoundaryLabelsEveryFigureAFloor() {
        var measurement = CostMeasurement.empty
        measurement.runsMeasured = 1
        measurement.runsTotal = 2
        measurement.totalTokens = 1_200
        let figure = TokenUsageFormatter.measurementTotal(measurement, spansCaptureBoundary: true)
        XCTAssertEqual(figure.annotation, "floor")
    }

    func testUsageUnavailableCaveatIsTheReasonNotAGrokSpecificMessage() {
        let caveat = UsageCaveat(
            code: "usage_unavailable",
            detail: "No provider token usage observed"
        )
        XCTAssertEqual(
            TokenUsageFormatter.caveatLabel(code: caveat.code, detail: caveat.detail),
            "No provider token usage observed"
        )
        XCTAssertEqual(TokenUsageFormatter.caveatSentence(caveat), "No provider token usage observed")
        XCTAssertFalse(TokenUsageFormatter.caveatSentence(caveat).localizedCaseInsensitiveContains("grok"))
    }

    func testUnknownCaveatCodeIsShownVerbatim() {
        XCTAssertEqual(
            TokenUsageFormatter.caveatLabel(code: "capture_skipped", detail: "containment"),
            "capture_skipped: containment"
        )
        XCTAssertEqual(
            TokenUsageFormatter.caveatSentence(UsageCaveat(code: "capture_skipped", detail: nil)),
            "capture_skipped"
        )
    }

    func testCaptureSceneParsesFromFlag() {
        XCTAssertEqual(
            TokenUsageCapture.parseScene(from: ["Boss", "--capture-token-usage-scene", "grouped"]),
            .grouped
        )
        XCTAssertEqual(
            TokenUsageCapture.parseScene(from: ["Boss", "--capture-token-usage-scene=inspector"]),
            .inspector
        )
        XCTAssertEqual(
            TokenUsageCapture.parseScene(from: ["Boss", "--capture-token-usage-scene=capture-boundary"]),
            .captureBoundary
        )
        XCTAssertNil(TokenUsageCapture.parseScene(from: ["Boss", "--capture-token-usage"]))
    }

    func testFourDriverLabelsIncludeAntigravity() {
        XCTAssertEqual(TokenUsageFormatter.driverLabel("claude"), "Claude")
        XCTAssertEqual(TokenUsageFormatter.driverLabel("codex"), "Codex")
        XCTAssertEqual(TokenUsageFormatter.driverLabel("grok"), "Grok")
        XCTAssertEqual(TokenUsageFormatter.driverLabel("agy"), "Antigravity")
        XCTAssertEqual(TokenUsageFormatter.driverLabel(nil), "not recorded")
        XCTAssertEqual(
            TokenUsageFormatter.driverLabel("claude", source: .resolved),
            "Claude (current routing)"
        )
    }

    func testDurationIsUnitConversionOnly() {
        XCTAssertEqual(TokenUsageFormatter.duration(ms: nil), "—")
        XCTAssertEqual(TokenUsageFormatter.duration(ms: 0), "0s")
        XCTAssertEqual(TokenUsageFormatter.duration(ms: 1_260_000), "21m")
        XCTAssertEqual(TokenUsageFormatter.duration(ms: 3_600_000), "1h")
    }

    func testCoverageBadgeFromMeasurementUsesEngineCounts() {
        var complete = CostMeasurement.empty
        complete.runsMeasured = 4
        complete.runsTotal = 4
        XCTAssertEqual(TokenUsageFormatter.coverageBadge(measurement: complete), "complete")

        var partial = CostMeasurement.empty
        partial.runsMeasured = 2
        partial.runsTotal = 3
        partial.runsUnmeasured = 1
        partial.tokensPartial = true
        XCTAssertEqual(TokenUsageFormatter.coverageBadge(measurement: partial), "partial 2/3")

        var none = CostMeasurement.empty
        none.runsTotal = 2
        none.runsUnmeasured = 2
        XCTAssertEqual(
            TokenUsageFormatter.coverageBadge(measurement: none, reason: "not recorded"),
            "none (not recorded)"
        )
    }

    func testCoverageBadgeCompletePartialNone() {
        XCTAssertEqual(
            TokenUsageFormatter.coverageBadge(complete: 4, partial: 0, zero: 0, unmeasured: 0, total: 4),
            "complete"
        )
        XCTAssertEqual(
            TokenUsageFormatter.coverageBadge(complete: 2, partial: 1, zero: 0, unmeasured: 1, total: 4),
            "partial 3/4"
        )
        XCTAssertEqual(
            TokenUsageFormatter.coverageBadge(
                complete: 0, partial: 0, zero: 0, unmeasured: 2, total: 2, reason: "not recorded"
            ),
            "none (not recorded)"
        )
    }
}
