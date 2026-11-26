import XCTest
@testable import Boss

final class TokenUsageSeriesMapperTests: XCTestCase {
    // 2026-11-02T00:00:00Z, a Monday; a day is 86_400 s.
    private let day0: Int64 = 1_793_577_600
    private let day: Int64 = 86_400

    // MARK: - Fixtures

    private var utcCalendar: Calendar {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(secondsFromGMT: 0)!
        return calendar
    }

    /// Chart space is UTC here so expectations do not depend on the host zone.
    private func makeMapper(_ report: TokenUsageSeriesReport) -> UsageSeriesMapper {
        UsageSeriesMapper(report: report, plotCalendar: utcCalendar)
    }

    private func measurement(
        measured: UInt32 = 2,
        total: UInt32 = 2,
        tokens: Int64 = 1_000,
        input: Int64? = 400,
        output: Int64? = 300,
        cacheWrite: Int64? = 100,
        cacheRead: Int64? = 200,
        partial: Bool = false,
        usd: Double? = nil,
        usdPartial: Bool = false
    ) -> CostMeasurement {
        CostMeasurement(
            usageUnavailable: [],
            tokensPartial: partial,
            agentActiveMs: 0,
            cacheCreationTokens: cacheWrite,
            cacheReadTokens: cacheRead,
            estimatedUsdPartial: usdPartial,
            inputTokens: input,
            outputTokens: output,
            runsMeasured: measured,
            runsTotal: total,
            runsUnmeasured: total - measured,
            runsZero: 0,
            totalTokens: tokens,
            estimatedUsd: usd
        )
    }

    private func cell(
        _ group: String = "(all)",
        measurement: CostMeasurement? = nil,
        ratios: UsageRatios = UsageRatios(),
        share: Double? = nil,
        executions: UInt32 = 1,
        tasks: UInt32 = 0,
        prs: UInt32 = 0
    ) -> UsageCell {
        UsageCell(
            distinctPrs: prs,
            executionsCompleted: executions,
            group: group,
            measurement: measurement ?? self.measurement(),
            ratios: ratios,
            tasksCompleted: tasks,
            tasksCompletedCost: .empty,
            shareOfBucketTotalTokens: share
        )
    }

    private func report(
        buckets: [(Int64, [UsageCell])],
        groups: [String] = ["(all)"],
        groupBy: UsageDimension? = nil,
        labels: [String: String] = [:],
        width: UsageBucketWidth = .day,
        since: Int64? = nil,
        until: Int64? = nil,
        notes: [UsageSeriesNote] = [],
        spansCapture: Bool = false,
        captureStart: Int64 = 0,
        dimensionFrom: Int64? = nil,
        excludedFromActiveHour: UInt32 = 0
    ) -> TokenUsageSeriesReport {
        TokenUsageSeriesReport(
            bucketSecs: width == .day ? 86_400 : (width == .hour ? 3_600 : (width == .week ? 604_800 : 2_592_000)),
            bucketWidth: width,
            captureStartEpochS: captureStart,
            coverage: UsageSeriesCoverage(
                byCaveat: [],
                notes: notes,
                runsExcludedFromActiveHour: excludedFromActiveHour,
                dataFromEpochS: nil,
                dimensionFromEpochS: dimensionFrom
            ),
            generatedAtEpochS: until ?? day0 + 10 * day,
            sinceEpochS: since ?? day0,
            spansCaptureBoundary: spansCapture,
            untilEpochS: until ?? day0 + 10 * day,
            buckets: buckets.map { UsageBucket(startEpochS: $0.0, cells: $0.1) },
            groups: groups,
            pricingGaps: [],
            groupBy: groupBy,
            groupLabels: labels
        )
    }

    // MARK: - Decoding

    func testParseKeepsGroupLabelKeysVerbatimAndNullRatiosNil() throws {
        let json = """
        {
          "bucket_secs": 86400, "bucket_width": "day", "capture_start_epoch_s": 5,
          "coverage": {"by_caveat": [{"code": "gross_input", "runs": 2}],
                       "notes": [{"code": "capture_started", "epoch_s": 5, "detail": "d"}],
                       "runs_excluded_from_active_hour": 3,
                       "data_from_epoch_s": 5, "dimension_from_epoch_s": null},
          "generated_at_epoch_s": 9, "since_epoch_s": 1, "spans_capture_boundary": true,
          "until_epoch_s": 9, "group_by": "task_kind",
          "groups": ["chore_implementation"], "pricing_gaps": [],
          "group_labels": {"chore_implementation": "Chore implementation"},
          "buckets": [{"start_epoch_s": 0, "cells": [{
            "distinct_prs": 1, "executions_completed": 2, "group": "chore_implementation",
            "measurement": {"usage_unavailable": [], "tokens_partial": false, "agent_active_ms": 0,
              "cache_creation_tokens": null, "cache_read_tokens": null, "estimated_usd_partial": false,
              "input_tokens": null, "output_tokens": null, "runs_measured": 0, "runs_total": 3,
              "runs_unmeasured": 3, "runs_zero": 0, "total_tokens": 0, "estimated_usd": null},
            "ratios": {"tokens_per_active_hour": null, "tokens_per_completed_execution": null,
              "tokens_per_completed_task": null, "tokens_per_pr": null, "tokens_per_run": 12.5},
            "tasks_completed": 0, "tasks_completed_cost": {"usage_unavailable": [], "tokens_partial": false,
              "agent_active_ms": 0, "cache_creation_tokens": null, "cache_read_tokens": null,
              "estimated_usd_partial": false, "input_tokens": null, "output_tokens": null, "runs_measured": 0,
              "runs_total": 0, "runs_unmeasured": 0, "runs_zero": 0, "total_tokens": 0, "estimated_usd": null},
            "share_of_bucket_total_tokens": null}]}]
        }
        """
        let object = try JSONSerialization.jsonObject(with: Data(json.utf8))
        let parsed = try XCTUnwrap(TokenUsageSeriesReport.parse(object))
        XCTAssertEqual(parsed.groupBy, .taskKind)
        XCTAssertEqual(parsed.groupLabels["chore_implementation"], "Chore implementation")
        XCTAssertEqual(parsed.coverage.runsExcludedFromActiveHour, 3)
        let decoded = try XCTUnwrap(parsed.buckets.first?.cells.first)
        XCTAssertNil(decoded.ratios.tokensPerActiveHour)
        XCTAssertEqual(decoded.ratios.tokensPerRun, 12.5)
        XCTAssertNil(decoded.shareOfBucketTotalTokens)
        XCTAssertNil(decoded.measurement.inputTokens, "an unreported class stays nil, never 0")
    }

    func testParseRejectsAPayloadThatIsNotAnObject() {
        XCTAssertNil(TokenUsageSeriesReport.parse([1, 2, 3]))
    }

    // MARK: - Geometry

    func testDayBucketsSpanOneDayAndMonthBucketsFollowTheCalendar() {
        let dayMapper = makeMapper(report(buckets: [(day0, [cell()])]))
        XCTAssertEqual(dayMapper.spans[0].end.timeIntervalSince(dayMapper.spans[0].start), 86_400)
        // 2026-02-01T00:00:00Z is 1_769_904_000; February 2026 has 28 days.
        let monthMapper = makeMapper(report(buckets: [(1_769_904_000, [cell()])], width: .month))
        XCTAssertEqual(monthMapper.spans[0].end.timeIntervalSince(monthMapper.spans[0].start), 28 * 86_400)
    }

    func testDomainStartsAtTheFirstBucketNotTheRequestedEpochAndReachesUntil() throws {
        let mapper = makeMapper(report(
            buckets: [(day0, [cell()]), (day0 + 2 * day, [cell()])],
            since: 0,
            until: day0 + 9 * day
        ))
        let domain = try XCTUnwrap(mapper.domain)
        XCTAssertEqual(domain.lowerBound, UsageSeriesGeometry.date(day0))
        XCTAssertEqual(domain.upperBound, UsageSeriesGeometry.date(day0 + 9 * day))
    }

    // MARK: - Tokens card

    func testUnmeasuredBucketIsFlaggedNoDataAndNeverDrawnAsAZeroBar() {
        let unmeasured = measurement(measured: 0, total: 3, tokens: 0, input: nil, output: nil, cacheWrite: nil, cacheRead: nil)
        let mapper = makeMapper(report(buckets: [
            (day0, [cell(measurement: unmeasured)]),
            (day0 + day, [cell()]),
        ]))
        XCTAssertEqual(mapper.tokenBarsByGroup().map(\.bucketIndex), [1])
        XCTAssertTrue(mapper.tokenBarsByClass().allSatisfy { $0.bucketIndex == 1 })
        let flags = mapper.tokenFlags(byClass: false)
        XCTAssertEqual(flags.map(\.kind), [.noData])
        XCTAssertEqual(flags.first?.bucketIndex, 0)
        XCTAssertTrue(flags.first?.detail.contains("not zero") == true)
    }

    func testMeasuredZeroIsDrawnAsAZeroBarNotFlagged() {
        let zero = measurement(measured: 1, total: 1, tokens: 0, input: 0, output: 0, cacheWrite: 0, cacheRead: 0)
        let mapper = makeMapper(report(buckets: [(day0, [cell(measurement: zero)])]))
        XCTAssertEqual(mapper.tokenBarsByGroup().map(\.value), [0])
        XCTAssertTrue(mapper.tokenFlags(byClass: false).isEmpty)
    }

    func testPartialBucketKeepsItsBarAsAFloorAndNamesTheMissingClass() {
        let partial = measurement(cacheWrite: nil, partial: true)
        let mapper = makeMapper(report(buckets: [(day0, [cell(measurement: partial)])]))
        XCTAssertEqual(mapper.tokenBarsByGroup().map(\.value), [1_000])
        let byClass = mapper.tokenBarsByClass()
        XCTAssertEqual(Set(byClass.map(\.series)), ["Input", "Output", "Cache read"], "cache write is unknown, so it has no segment")
        let flag = mapper.tokenFlags(byClass: true).first
        XCTAssertEqual(flag?.kind, .floor)
        XCTAssertTrue(flag?.detail.contains("cache write") == true)
        XCTAssertEqual(mapper.hoverTokensByClass(0).rows.first { $0.label == "Cache write" }?.value, "unknown")
    }

    func testMixedMeasuredAndUnmeasuredRunsFlagAFloor() {
        let mixed = measurement(measured: 2, total: 5)
        let mapper = makeMapper(report(buckets: [(day0, [cell(measurement: mixed)])]))
        let flag = mapper.tokenFlags(byClass: false).first
        XCTAssertEqual(flag?.kind, .floor)
        XCTAssertTrue(flag?.detail.contains("3 of 5 runs") == true)
    }

    func testAntigravityDriverGroupGetsItsDisplayNameEvenWithoutAnEngineLabel() {
        let mapper = makeMapper(report(
            buckets: [(day0, [cell("agy")])],
            groups: ["agy"],
            groupBy: .driver
        ))
        XCTAssertEqual(mapper.groupLabels, ["Antigravity"])
    }

    func testGroupedTokensAndClassModeIsUnavailable() {
        let mapper = makeMapper(report(
            buckets: [(day0, [cell("claude"), cell("agy", measurement: measurement(tokens: 50, cacheWrite: nil, partial: true))])],
            groups: ["claude", "agy"],
            groupBy: .driver,
            labels: ["claude": "Claude", "agy": "Antigravity"]
        ))
        XCTAssertEqual(mapper.tokenBarsByGroup().map(\.series), ["Claude", "Antigravity"])
        XCTAssertTrue(mapper.tokenBarsByClass().isEmpty)
        XCTAssertEqual(mapper.groupLabels, ["Claude", "Antigravity"])
    }

    func testAGroupWithNoMeasuredRunsInAMeasuredBucketFlagsAFloorWithItsLabel() {
        let none = measurement(measured: 0, total: 1, tokens: 0, input: nil, output: nil, cacheWrite: nil, cacheRead: nil)
        let mapper = makeMapper(report(
            buckets: [(day0, [cell("claude"), cell("grok", measurement: none)])],
            groups: ["claude", "grok"],
            groupBy: .driver,
            labels: ["claude": "Claude", "grok": "Grok"]
        ))
        XCTAssertEqual(mapper.tokenBarsByGroup().map(\.series), ["Claude"])
        let flag = mapper.tokenFlags(byClass: false).first
        XCTAssertEqual(flag?.kind, .floor)
        XCTAssertTrue(flag?.detail.contains("Grok") == true)
    }

    // MARK: - Work volume

    func testUngroupedWorkOverviewHasOneBarPerCountAtTheBucketStart() {
        let mapper = makeMapper(report(buckets: [(day0, [cell(executions: 3, tasks: 2, prs: 1)])]))
        let bars = mapper.workBars(metric: nil)
        XCTAssertEqual(bars.map(\.series), UsageWorkMetric.allCases.map(\.title))
        XCTAssertEqual(bars.map(\.value), [2, 3, 2, 1])
        XCTAssertTrue(bars.allSatisfy { $0.x == mapper.spans[0].start })
    }

    func testGroupedWorkUsesOneBarPerGroupForTheChosenMetric() {
        let mapper = makeMapper(report(
            buckets: [(day0, [cell("claude", executions: 4), cell("codex", executions: 1)])],
            groups: ["claude", "codex"],
            groupBy: .driver
        ))
        let bars = mapper.workBars(metric: .executionsCompleted)
        XCTAssertEqual(bars.map(\.series), ["Claude", "Codex"], "driver slugs get their display names")
        XCTAssertEqual(bars.map(\.value), [4, 1])
        XCTAssertEqual(bars[0].x, bars[1].x, "groups stack at the same bucket")
    }

    // MARK: - Normalized

    func testAbsentBucketAndNilRatioBreakTheLineIntoSegments() {
        func ratios(_ perRun: Double?) -> UsageRatios { UsageRatios(tokensPerRun: perRun) }
        let mapper = makeMapper(report(buckets: [
            (day0, [cell(ratios: ratios(10))]),
            (day0 + day, [cell(ratios: ratios(20))]),
            // day 2 absent: no activity
            (day0 + 3 * day, [cell(ratios: ratios(30))]),
            (day0 + 4 * day, [cell(ratios: ratios(nil))]),
            (day0 + 5 * day, [cell(ratios: ratios(50))]),
        ]))
        let segments = mapper.normalizedSegments(.perRun)
        XCTAssertEqual(segments.map { $0.points.map(\.y) }, [[10, 20], [30], [50]])
        XCTAssertEqual(segments.map(\.index), [0, 1, 2])
        XCTAssertEqual(Set(segments.map(\.id)).count, 3)
    }

    func testEachNormalizerReadsItsOwnEngineRatio() {
        let ratios = UsageRatios(
            tokensPerActiveHour: 1,
            tokensPerCompletedExecution: 2,
            tokensPerCompletedTask: 3,
            tokensPerPr: 4,
            tokensPerRun: 5
        )
        let mapper = makeMapper(report(buckets: [(day0, [cell(ratios: ratios)])]))
        let values = UsageNormalizer.allCases.map { mapper.normalizedSegments($0).first?.points.first?.y }
        XCTAssertEqual(values, [5, 2, 1, 3, 4])
    }

    func testGroupedLinesAreSeparatePerGroupInRenderOrder() {
        let mapper = makeMapper(report(
            buckets: [(day0, [cell("a", ratios: UsageRatios(tokensPerRun: 1)), cell("b", ratios: UsageRatios(tokensPerRun: 2))])],
            groups: ["b", "a"],
            groupBy: .model,
            labels: ["a": "A", "b": "B"]
        ))
        XCTAssertEqual(mapper.normalizedSegments(.perRun).map(\.series), ["B", "A"])
    }

    func testActiveHourCaptionStatesHowManyRunsWereLeftOut() {
        let mapper = makeMapper(report(buckets: [(day0, [cell()])], excludedFromActiveHour: 4))
        XCTAssertTrue(mapper.normalizerCaption(.perActiveHour).contains("4 measured runs"))
        let none = makeMapper(report(buckets: [(day0, [cell()])]))
        XCTAssertFalse(none.normalizerCaption(.perActiveHour).contains("measured run"))
        XCTAssertTrue(none.normalizerCaption(.perCompletedTask).contains("Does not add up"))
    }

    // MARK: - Mix and USD

    func testMixNeedsGroupingAndOmitsGroupsWithoutAShare() {
        let ungrouped = makeMapper(report(buckets: [(day0, [cell(share: 1)])]))
        XCTAssertTrue(ungrouped.mixBars().isEmpty)
        let grouped = makeMapper(report(
            buckets: [(day0, [cell("a", share: 0.75), cell("b", share: 0.25), cell("c", share: nil)])],
            groups: ["a", "b", "c"],
            groupBy: .model
        ))
        XCTAssertEqual(grouped.mixBars().map(\.value), [0.75, 0.25])
        XCTAssertEqual(grouped.mixFlags().first?.kind, .floor)
        XCTAssertEqual(grouped.hoverMix(0).rows.last?.value, "no measured runs")
    }

    func testUnpricedBucketsAreFlaggedNotDrawnAsZeroDollars() {
        let priced = measurement(usd: 1.5, usdPartial: true)
        let unpriced = measurement(usd: nil)
        let mapper = makeMapper(report(buckets: [
            (day0, [cell(measurement: priced)]),
            (day0 + day, [cell(measurement: unpriced)]),
        ]))
        XCTAssertEqual(mapper.usdBars().map(\.value), [1.5])
        XCTAssertEqual(mapper.usdFlags().map(\.kind), [.floor, .noData])
        XCTAssertEqual(mapper.hoverUsd(1).rows.first?.value, "not priced")
        XCTAssertTrue(mapper.hoverUsd(0).rows.first?.value.hasPrefix("est. $1.50") == true)
    }

    // MARK: - Shading

    func testCaptureBoundaryShadesFromTheDomainStartToTheNoteEpoch() throws {
        let mapper = makeMapper(report(
            buckets: [(day0, [cell()]), (day0 + 5 * day, [cell()])],
            since: 0,
            notes: [UsageSeriesNote(code: "capture_started", epochS: day0 + 2 * day, detail: "x")],
            spansCapture: true,
            captureStart: day0 + 2 * day
        ))
        let shade = try XCTUnwrap(mapper.shades(includeCompletedTask: false).first)
        XCTAssertEqual(shade.from, UsageSeriesGeometry.date(day0))
        XCTAssertEqual(shade.to, UsageSeriesGeometry.date(day0 + 2 * day))
        XCTAssertTrue(shade.label.hasPrefix("No token data before"))
    }

    func testNoShadeWhenTheRangeStartsAfterCapture() {
        let mapper = makeMapper(report(buckets: [(day0, [cell()])], captureStart: day0 - day))
        XCTAssertTrue(mapper.shades(includeCompletedTask: true).isEmpty)
    }

    func testDimensionAndCompletedTaskNotesAreToldApartByTheDimensionEpoch() {
        let notes = [
            UsageSeriesNote(code: "dimension_started", epochS: day0 + 3 * day, detail: "completed_at migration"),
            UsageSeriesNote(code: "dimension_started", epochS: day0 + day, detail: "driver coverage"),
        ]
        let mapper = makeMapper(report(
            buckets: [(day0, [cell()]), (day0 + 6 * day, [cell()])],
            groupBy: .driver,
            notes: notes,
            dimensionFrom: day0 + day
        ))
        let without = mapper.shades(includeCompletedTask: false)
        XCTAssertEqual(without.count, 1)
        XCTAssertTrue(without[0].label.hasPrefix("Driver not recorded before"))
        let with = mapper.shades(includeCompletedTask: true)
        XCTAssertEqual(with.count, 2)
        XCTAssertTrue(with[1].label.hasPrefix("Completed-task times backfilled"))
        XCTAssertEqual(with[1].to, UsageSeriesGeometry.date(day0 + 3 * day))
    }

    func testAShadeBeyondTheDomainIsClippedToIt() throws {
        let mapper = makeMapper(report(
            buckets: [(day0, [cell()])],
            until: day0 + day,
            notes: [UsageSeriesNote(code: "capture_started", epochS: day0 + 100 * day, detail: "x")],
            spansCapture: true,
            captureStart: day0 + 100 * day
        ))
        let shade = try XCTUnwrap(mapper.shades(includeCompletedTask: false).first)
        XCTAssertEqual(shade.to, mapper.domain?.upperBound)
    }

    // MARK: - Hover

    func testBucketIndexPicksTheContainingBucketThenTheNearest() {
        let mapper = makeMapper(report(buckets: [(day0, [cell()]), (day0 + 5 * day, [cell()])]))
        XCTAssertEqual(mapper.bucketIndex(nearest: UsageSeriesGeometry.date(day0 + 3_600)), 0)
        XCTAssertEqual(mapper.bucketIndex(nearest: UsageSeriesGeometry.date(day0 + 4 * day)), 1, "a gap resolves to the nearest bucket")
        XCTAssertNil(makeMapper(report(buckets: [])).bucketIndex(nearest: Date()))
    }

    func testHoverReadoutsShowExactFiguresUnknownsAndNoValue() {
        let unmeasured = measurement(measured: 0, total: 2, tokens: 0, input: nil, output: nil, cacheWrite: nil, cacheRead: nil)
        let mapper = makeMapper(report(
            buckets: [(day0, [cell("a", measurement: measurement(tokens: 1_234_567)), cell("b", measurement: unmeasured)])],
            groups: ["a", "b"],
            groupBy: .driver
        ))
        let rows = mapper.hoverTokensByGroup(0).rows
        XCTAssertEqual(rows.map(\.value), ["1,234,567", "no token data"])
        XCTAssertEqual(mapper.hoverNormalized(0, normalizer: .perRun).rows.map(\.value), ["no value", "no value"])
        XCTAssertTrue(mapper.hoverTokensByGroup(0).title.contains("2026"))
    }

    func testChartCalendarAlignsUnitsToEngineBuckets() {
        XCTAssertEqual(UsageSeriesGeometry.unit(.week), .weekOfYear)
        // The epoch (and so every engine week) starts on a Thursday.
        let calendar = UsageSeriesGeometry.chartCalendar(.week, base: utcCalendar)
        XCTAssertEqual(calendar.firstWeekday, 5)
        XCTAssertEqual(UsageSeriesGeometry.chartCalendar(.day, base: utcCalendar).firstWeekday, utcCalendar.firstWeekday)
        let start = UsageSeriesGeometry.date(604_800 * 2_900)
        XCTAssertEqual(calendar.dateInterval(of: .weekOfYear, for: start)?.start, start)
    }

    func testPlotSpaceKeepsTheUtcWallClockInTheViewersTimeZone() throws {
        var pacific = Calendar(identifier: .gregorian)
        pacific.timeZone = try XCTUnwrap(TimeZone(identifier: "America/Los_Angeles"))
        let mapper = UsageSeriesMapper(
            report: report(buckets: [(day0, [cell(ratios: UsageRatios(tokensPerRun: 1))])]),
            plotCalendar: pacific
        )
        // 2026-11-02T00:00Z must read as midnight Nov 2 on the viewer's calendar,
        // or a calendar-unit bar would land on Nov 1.
        let components = pacific.dateComponents([.year, .month, .day, .hour], from: mapper.plotSpans[0].start)
        XCTAssertEqual([components.year, components.month, components.day, components.hour], [2026, 11, 2, 0])
        XCTAssertEqual(mapper.tokenBarsByGroup().first?.x, mapper.plotSpans[0].start)
        XCTAssertEqual(mapper.normalizedSegments(.perRun).first?.points.first?.x, mapper.plotSpans[0].mid)
        // Real time is untouched, and a selection in chart space maps back to its bucket.
        XCTAssertEqual(mapper.spans[0].start, UsageSeriesGeometry.date(day0))
        XCTAssertEqual(mapper.bucketIndex(nearest: mapper.plotSpans[0].mid), 0)
    }

    func testCompactNumbersForAxes() {
        XCTAssertEqual(UsageSeriesNumber.compact(950), "950")
        XCTAssertEqual(UsageSeriesNumber.compact(1_500), "1.5k")
        XCTAssertEqual(UsageSeriesNumber.compact(2_000_000), "2M")
        XCTAssertEqual(UsageSeriesNumber.compact(12_400_000_000), "12.4B")
        XCTAssertEqual(UsageSeriesNumber.percent(0.256), "26%")
    }

    // MARK: - Request state and capture scenes

    func testSeriesRequestStateDropsStaleAndUntaggedReplies() {
        var state = TokenUsageSeriesSessionState()
        let current = report(buckets: [(day0, [cell()])])
        XCTAssertNotNil(state.beginRequest(requestId: "req-1"))
        XCTAssertFalse(state.receive(current, requestId: nil))
        XCTAssertNotNil(state.beginRequest(requestId: "req-2"))
        XCTAssertFalse(state.receive(current, requestId: "req-1"), "a superseded request's reply is dropped")
        XCTAssertTrue(state.receive(current, requestId: "req-2"))
        XCTAssertEqual(state.report, current)
    }

    func testInvalidatingASeriesRequestRejectsItsReply() {
        var state = TokenUsageSeriesSessionState()
        _ = state.beginRequest(requestId: "req-1")
        state.invalidatePending()
        XCTAssertFalse(state.receive(report(buckets: []), requestId: "req-1"))
    }

    func testOverTimeCaptureScenesParse() {
        for scene in ["over-time", "over-time-grouped", "over-time-all", "over-time-hover"] {
            let parsed = TokenUsageCapture.parseScene(from: ["Boss", "--capture-token-usage-scene=\(scene)"])
            XCTAssertEqual(parsed?.rawValue, scene)
            XCTAssertEqual(parsed?.isOverTime, true)
        }
        XCTAssertEqual(TokenUsageCaptureScene.tree.isOverTime, false)
    }
}
