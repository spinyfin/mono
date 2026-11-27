import Foundation

// Pure mapping from a `TokenUsageSeriesReport` to what the Over time charts
// draw. Nothing here adds, divides or compares token counts: a point carries
// a field the engine computed, a missing field is an absent point plus a
// flag, and the only arithmetic is on dates (bucket spans, bar lanes, gap
// detection) and on cell/bar positions.

/// The five engine-computed normalizers, in picker order.
enum UsageNormalizer: String, CaseIterable, Identifiable, Sendable {
    case perRun
    case perCompletedExecution
    case perActiveHour
    case perCompletedTask
    case perPr

    var id: String { rawValue }

    var title: String {
        switch self {
        case .perRun: return "Per run"
        case .perCompletedExecution: return "Per completed execution"
        case .perActiveHour: return "Per active hour"
        case .perCompletedTask: return "Per completed task"
        case .perPr: return "Per PR"
        }
    }

    var unitLabel: String {
        switch self {
        case .perRun: return "tokens / run"
        case .perCompletedExecution: return "tokens / execution"
        case .perActiveHour: return "tokens / active hour"
        case .perCompletedTask: return "tokens / task"
        case .perPr: return "tokens / PR"
        }
    }

    func value(_ ratios: UsageRatios) -> Double? {
        switch self {
        case .perRun: return ratios.tokensPerRun
        case .perCompletedExecution: return ratios.tokensPerCompletedExecution
        case .perActiveHour: return ratios.tokensPerActiveHour
        case .perCompletedTask: return ratios.tokensPerCompletedTask
        case .perPr: return ratios.tokensPerPr
        }
    }
}

/// One of the four counts on the work-volume card.
enum UsageWorkMetric: String, CaseIterable, Identifiable, Sendable {
    case runs
    case executionsCompleted
    case tasksCompleted
    case distinctPrs

    var id: String { rawValue }

    var title: String {
        switch self {
        case .runs: return "Runs + slices"
        case .executionsCompleted: return "Executions completed"
        case .tasksCompleted: return "Tasks completed"
        case .distinctPrs: return "Distinct PRs"
        }
    }

    func count(_ cell: UsageCell) -> UInt32 {
        switch self {
        case .runs: return cell.measurement.runsTotal
        case .executionsCompleted: return cell.executionsCompleted
        case .tasksCompleted: return cell.tasksCompleted
        case .distinctPrs: return cell.distinctPrs
        }
    }
}

/// Which token-class series the tokens card stacks.
enum UsageTokenClass: String, CaseIterable, Sendable {
    case input = "Input"
    case output = "Output"
    case cacheWrite = "Cache write"
    case cacheRead = "Cache read"

    func tokens(_ measurement: CostMeasurement) -> Int64? {
        switch self {
        case .input: return measurement.inputTokens
        case .output: return measurement.outputTokens
        case .cacheWrite: return measurement.cacheCreationTokens
        case .cacheRead: return measurement.cacheReadTokens
        }
    }
}

struct UsageBucketSpan: Equatable, Identifiable, Sendable {
    var start: Date
    var end: Date
    var id: Date { start }

    var mid: Date {
        start.addingTimeInterval(end.timeIntervalSince(start) / 2)
    }
}

/// One drawn bar. `seriesKey` is the unique engine group key (or class/metric
/// identifier) used for identity and styling; `series` is the display label.
/// `x` is the bucket start, which the chart snaps to a calendar unit
/// (`UsageSeriesGeometry.chartCalendar`).
struct UsageBarPoint: Equatable, Identifiable, Sendable {
    var bucketIndex: Int
    var seriesKey: String
    var series: String
    var x: Date
    var value: Double

    var id: String { "\(bucketIndex)|\(seriesKey)" }
}

struct UsageLinePoint: Equatable, Sendable {
    var x: Date
    var y: Double
}

/// A run of consecutive buckets with a value. A gap (absent bucket or a
/// `None` ratio) always starts a new segment, so no line crosses it.
struct UsageLineSegment: Equatable, Identifiable, Sendable {
    var seriesKey: String
    var series: String
    var index: Int
    var points: [UsageLinePoint]

    var id: String { "\(seriesKey)|\(index)" }
}

enum UsageBucketFlagKind: Equatable, Sendable {
    /// Nothing in the bucket has a token figure: drawn as a marker, not a bar.
    case noData
    /// A figure exists but it is a floor: unmeasured runs or a class the
    /// provider did not report.
    case floor
}

struct UsageBucketFlag: Equatable, Identifiable, Sendable {
    var bucketIndex: Int
    var kind: UsageBucketFlagKind
    var at: Date
    var detail: String

    var id: Int { bucketIndex }
}

struct UsageShade: Equatable, Identifiable, Sendable {
    var from: Date
    var to: Date
    var label: String

    var id: String { "\(from.timeIntervalSince1970)|\(to.timeIntervalSince1970)|\(label)" }
}

struct UsageHoverRow: Equatable, Sendable {
    var label: String
    var value: String
    var seriesKey: String?
}

struct UsageHoverReadout: Equatable, Sendable {
    var title: String
    var rows: [UsageHoverRow]
}

enum UsageSeriesGeometry {
    private static let utc: Calendar = {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(secondsFromGMT: 0) ?? .gmt
        return calendar
    }()

    /// The calendar unit one bucket spans, for bar widths.
    static func unit(_ width: UsageBucketWidth) -> Calendar.Component {
        switch width {
        case .hour: return .hour
        case .day: return .day
        case .week: return .weekOfYear
        case .month: return .month
        }
    }

    /// `base` adjusted so its units line up with the engine's buckets. Engine
    /// week buckets start on Monday 00:00:00 UTC (`WEEK_EPOCH_OFFSET_S = 4 * 86_400`).
    static func chartCalendar(_ width: UsageBucketWidth, base: Calendar) -> Calendar {
        var calendar = base
        if width == .week {
            calendar.firstWeekday = 2
        }
        return calendar
    }

    /// The instant whose reading in `calendar` equals `date`'s reading in UTC.
    /// Identity when `calendar` is UTC.
    static func plotDate(_ date: Date, in calendar: Calendar) -> Date {
        let components = utc.dateComponents(
            [.year, .month, .day, .hour, .minute, .second, .nanosecond],
            from: date
        )
        return calendar.date(from: components) ?? date
    }

    static func date(_ epochS: Int64) -> Date {
        Date(timeIntervalSince1970: TimeInterval(epochS))
    }

    /// Exclusive end of a bucket. Hour, day and week are fixed lengths;
    /// month buckets run to the next UTC calendar month start.
    static func bucketEnd(startEpochS: Int64, width: UsageBucketWidth, bucketSecs: Int64) -> Date {
        let start = date(startEpochS)
        switch width {
        case .month:
            return utc.date(byAdding: .month, value: 1, to: start)
                ?? start.addingTimeInterval(TimeInterval(bucketSecs))
        case .hour, .day, .week:
            return start.addingTimeInterval(TimeInterval(bucketSecs))
        }
    }

    static func bucketLabel(_ start: Date, width: UsageBucketWidth) -> String {
        let style: Date.FormatStyle
        switch width {
        case .hour:
            style = Date.FormatStyle(timeZone: .gmt).month(.abbreviated).day().hour().minute()
        case .day:
            style = Date.FormatStyle(timeZone: .gmt).month(.abbreviated).day().year()
        case .week:
            style = Date.FormatStyle(timeZone: .gmt).month(.abbreviated).day().year()
        case .month:
            style = Date.FormatStyle(timeZone: .gmt).month(.wide).year()
        }
        let text = start.formatted(style)
        switch width {
        case .hour: return "\(text) UTC"
        case .day: return "\(text) (UTC day)"
        case .week: return "Week of \(text) UTC"
        case .month: return text
        }
    }
}

/// Compact axis and readout numbers (`1.2M`). Display only.
enum UsageSeriesNumber {
    static func compact(_ value: Double) -> String {
        let magnitude = abs(value)
        let (scaled, suffix): (Double, String)
        switch magnitude {
        case 1_000_000_000...: (scaled, suffix) = (value / 1_000_000_000, "B")
        case 1_000_000...: (scaled, suffix) = (value / 1_000_000, "M")
        case 1_000...: (scaled, suffix) = (value / 1_000, "k")
        default: (scaled, suffix) = (value, "")
        }
        if suffix.isEmpty {
            return value == value.rounded() ? String(Int64(value)) : String(format: "%.1f", value)
        }
        let rounded = abs(scaled) >= 100 ? String(format: "%.0f", scaled) : String(format: "%.1f", scaled)
        return rounded.hasSuffix(".0") ? String(rounded.dropLast(2)) + suffix : rounded + suffix
    }

    static func percent(_ share: Double) -> String {
        String(format: "%.0f%%", share * 100)
    }
}

struct UsageSeriesMapper {
    static let allGroupKey = "(all)"

    let report: TokenUsageSeriesReport
    /// Bucket spans in real (UTC) time. Used for contiguity and titles.
    let spans: [UsageBucketSpan]
    /// The same spans in chart space; see `plot(_:)`.
    let plotSpans: [UsageBucketSpan]
    let plotCalendar: Calendar
    private let duplicateRawLabels: Set<String>

    /// Swift Charts snaps `unit:` bars to calendar units in the viewer's
    /// time zone, ignoring any calendar passed through the environment, so a
    /// UTC midnight bucket would land on the previous local day. Everything
    /// drawn is therefore placed at the same *wall-clock* reading in
    /// `plotCalendar` (the viewer's calendar in the app, UTC in tests); the
    /// axis then reads the engine's UTC bucket boundaries correctly.
    init(report: TokenUsageSeriesReport, plotCalendar: Calendar = .current) {
        self.report = report
        self.plotCalendar = plotCalendar
        var rawCounts: [String: Int] = [:]
        for key in report.groups {
            rawCounts[Self.rawGroupLabel(key, report: report), default: 0] += 1
        }
        duplicateRawLabels = Set(rawCounts.filter { $0.value > 1 }.keys)
        spans = report.buckets.map { bucket in
            UsageBucketSpan(
                start: UsageSeriesGeometry.date(bucket.startEpochS),
                end: UsageSeriesGeometry.bucketEnd(
                    startEpochS: bucket.startEpochS,
                    width: report.bucketWidth,
                    bucketSecs: report.bucketSecs
                )
            )
        }
        plotSpans = spans.map { span in
            UsageBucketSpan(
                start: UsageSeriesGeometry.plotDate(span.start, in: plotCalendar),
                end: UsageSeriesGeometry.plotDate(span.end, in: plotCalendar)
            )
        }
    }

    func plot(_ date: Date) -> Date {
        UsageSeriesGeometry.plotDate(date, in: plotCalendar)
    }

    var isEmpty: Bool { report.buckets.isEmpty }
    var isGrouped: Bool { report.groupBy != nil }

    private static func rawGroupLabel(_ key: String, report: TokenUsageSeriesReport) -> String {
        if report.groupBy == .driver, key != Self.allGroupKey {
            if let engineLabel = report.groupLabels[key], !engineLabel.isEmpty, engineLabel != key {
                return engineLabel
            }
            if key == "(none)" {
                return "not recorded"
            }
            return TokenUsageFormatter.driverLabel(key)
        }
        if let label = report.groupLabels[key], !label.isEmpty { return label }
        return key == Self.allGroupKey ? "All" : key
    }

    func groupLabel(_ key: String) -> String {
        let raw = Self.rawGroupLabel(key, report: report)
        return duplicateRawLabels.contains(raw) ? "\(raw) (\(key))" : raw
    }

    /// Series labels in the engine's render order, so colours are stable
    /// across cards and across range changes that keep the same groups.
    var groupLabels: [String] { report.groups.map(groupLabel) }

    // MARK: - Domain and shading

    /// X domain. The lower bound is the first bucket, not the requested
    /// start, because "All" asks from the epoch.
    var domain: ClosedRange<Date>? {
        guard let first = spans.first, let last = spans.last else { return nil }
        let until = UsageSeriesGeometry.date(report.untilEpochS)
        return plot(first.start)...plot(max(last.end, until))
    }

    func shades(includeCompletedTask: Bool) -> [UsageShade] {
        guard let domain else { return [] }
        var shades: [UsageShade] = []
        func add(to epochS: Int64, _ label: String) {
            let to = min(plot(UsageSeriesGeometry.date(epochS)), domain.upperBound)
            if to > domain.lowerBound {
                shades.append(UsageShade(from: domain.lowerBound, to: to, label: label))
            }
        }
        let notes = report.coverage.notes
        let captureEpoch = notes.first { $0.code == "capture_started" }?.epochS
            ?? (report.spansCaptureBoundary ? report.captureStartEpochS : nil)
        if let captureEpoch {
            let date = TokenUsageFormatter.utcDateString(epochS: captureEpoch)
            add(to: captureEpoch, "No token data before \(date)")
        }
        // Two `dimension_started` notes can exist: the grouping dimension's
        // first recorded value (matches `dimension_from_epoch_s`) and the
        // `tasks.completed_at` migration. Tell them apart by that match.
        let dimensionIndex: Int? = report.groupBy == nil ? nil : notes.firstIndex {
            $0.code == "dimension_started"
                && $0.epochS != nil
                && $0.epochS == report.coverage.dimensionFromEpochS
        }
        if let dimensionIndex, let epoch = notes[dimensionIndex].epochS, let dimension = report.groupBy {
            add(to: epoch, "\(dimension.pickerLabel) not recorded before \(TokenUsageFormatter.utcDateString(epochS: epoch))")
        }
        if includeCompletedTask {
            for (index, note) in notes.enumerated()
            where note.code == "dimension_started" && index != dimensionIndex {
                if let epoch = note.epochS {
                    add(to: epoch, "Completed-task times backfilled before \(TokenUsageFormatter.utcDateString(epochS: epoch))")
                }
            }
        }
        return shades
    }

    /// Engine note text for the footer, in report order.
    var noteDetails: [String] {
        report.coverage.notes.map(\.detail)
    }

    // MARK: - Tokens card

    func tokenBarsByGroup() -> [UsageBarPoint] {
        var points: [UsageBarPoint] = []
        for (index, bucket) in report.buckets.enumerated() {
            for cell in bucket.cells where cell.measurement.runsMeasured > 0 {
                points.append(bar(index, seriesKey: cell.group, series: groupLabel(cell.group), value: Double(cell.measurement.totalTokens)))
            }
        }
        return points
    }

    /// Class series come from the bucket's single ungrouped cell. A class
    /// the provider did not report is simply not drawn (and flagged), never
    /// a zero-height segment.
    func tokenBarsByClass() -> [UsageBarPoint] {
        guard !isGrouped else { return [] }
        var points: [UsageBarPoint] = []
        for (index, bucket) in report.buckets.enumerated() {
            guard let cell = bucket.cells.first, cell.measurement.runsMeasured > 0 else { continue }
            for tokenClass in UsageTokenClass.allCases {
                if let tokens = tokenClass.tokens(cell.measurement) {
                    points.append(bar(index, seriesKey: tokenClass.rawValue, series: tokenClass.rawValue, value: Double(tokens)))
                }
            }
        }
        return points
    }

    func tokenFlags(byClass: Bool) -> [UsageBucketFlag] {
        flags { cell in
            if cell.measurement.runsMeasured == 0 {
                return .noData(unmeasured: cell.measurement)
            }
            var reasons: [String] = []
            if cell.measurement.runsMeasured < cell.measurement.runsTotal {
                reasons.append("\(cell.measurement.runsUnmeasured) of \(cell.measurement.runsTotal) runs have no token data")
            }
            if cell.measurement.tokensPartial {
                var missing = UsageTokenClass.allCases
                    .filter { $0.tokens(cell.measurement) == nil }
                    .map { $0.rawValue.lowercased() }
                if missing.isEmpty { missing = ["a token class"] }
                reasons.append("\(missing.joined(separator: ", ")) not reported by the provider")
            } else if byClass {
                let missing = UsageTokenClass.allCases.filter { $0.tokens(cell.measurement) == nil }
                if !missing.isEmpty {
                    reasons.append("\(missing.map { $0.rawValue.lowercased() }.joined(separator: ", ")) unknown")
                }
            }
            return reasons.isEmpty ? .complete : .floor(reasons.joined(separator: "; "))
        }
    }

    // MARK: - Work volume card

    func workCaption(metric: UsageWorkMetric?) -> String {
        let scope = metric.map { "\($0.title) by group." }
            ?? "Runs, completed executions, completed tasks and distinct PRs."
        return metric == nil || metric == .runs
            ? scope + " Runs + slices includes worker runs and coordinator hourly/model slices."
            : scope
    }

    /// Ungrouped overview: the four counts as side-by-side bars per bucket.
    /// With a metric: one stacked bar per group.
    func workBars(metric: UsageWorkMetric?) -> [UsageBarPoint] {
        var points: [UsageBarPoint] = []
        for (index, bucket) in report.buckets.enumerated() {
            for cell in bucket.cells {
                if let metric {
                    points.append(bar(index, seriesKey: cell.group, series: groupLabel(cell.group), value: Double(metric.count(cell))))
                } else {
                    for each in UsageWorkMetric.allCases {
                        points.append(bar(index, seriesKey: each.title, series: each.title, value: Double(each.count(cell))))
                    }
                }
            }
        }
        return points
    }

    // MARK: - Normalized card

    func normalizedSegments(_ normalizer: UsageNormalizer) -> [UsageLineSegment] {
        var segments: [UsageLineSegment] = []
        for key in report.groups {
            let label = groupLabel(key)
            var current: [UsageLinePoint] = []
            var previousEnd: Date?
            var segmentIndex = 0
            func close() {
                if !current.isEmpty {
                    segments.append(UsageLineSegment(seriesKey: key, series: label, index: segmentIndex, points: current))
                    segmentIndex += 1
                    current = []
                }
            }
            for (index, bucket) in report.buckets.enumerated() {
                let span = spans[index]
                let value = bucket.cells.first { $0.group == key }.flatMap { normalizer.value($0.ratios) }
                guard let value else {
                    close()
                    previousEnd = nil
                    continue
                }
                if let previousEnd, previousEnd != span.start {
                    close()
                }
                current.append(UsageLinePoint(x: plotSpans[index].mid, y: value))
                previousEnd = span.end
            }
            close()
        }
        return segments
    }

    /// What the selected normalizer means and what it leaves out, from
    /// engine-provided coverage.
    func normalizerCaption(_ normalizer: UsageNormalizer) -> String {
        switch normalizer {
        case .perRun:
            return "Measured worker tokens divided by measured worker runs in the bucket. Coordinator hourly/model slices are excluded."
        case .perCompletedExecution:
            return "Measured tokens in the bucket divided by executions that completed in it."
        case .perActiveHour:
            let excluded = report.coverage.runsExcludedFromActiveHour
            let base = "Tokens per hour of agent active time."
            if excluded > 0 {
                return "\(base) \(excluded) measured run\(excluded == 1 ? "" : "s") without recorded active time (for example Grok) are left out of both sides."
            }
            return base
        case .perCompletedTask:
            return "All-in cost of tasks finished in the bucket (including runs from earlier buckets), divided by those tasks. Does not add up across buckets."
        case .perPr:
            return "Approximation: measured tokens in the bucket divided by distinct PRs of executions that finished in it."
        }
    }

    // MARK: - Mix card

    func mixBars() -> [UsageBarPoint] {
        guard isGrouped else { return [] }
        var points: [UsageBarPoint] = []
        for (index, bucket) in report.buckets.enumerated() {
            for cell in bucket.cells {
                if let share = cell.shareOfBucketTotalTokens {
                    points.append(bar(index, seriesKey: cell.group, series: groupLabel(cell.group), value: share))
                }
            }
        }
        return points
    }

    func mixFlags() -> [UsageBucketFlag] {
        flags { cell in
            cell.shareOfBucketTotalTokens == nil
                ? .noData(unmeasured: cell.measurement)
                : .complete
        }
    }

    // MARK: - USD card

    func usdBars() -> [UsageBarPoint] {
        var points: [UsageBarPoint] = []
        for (index, bucket) in report.buckets.enumerated() {
            for cell in bucket.cells {
                if let usd = cell.measurement.estimatedUsd {
                    points.append(bar(index, seriesKey: cell.group, series: groupLabel(cell.group), value: usd))
                }
            }
        }
        return points
    }

    func usdFlags() -> [UsageBucketFlag] {
        flags { cell in
            if cell.measurement.estimatedUsd == nil {
                // Per engine fold.rs, subscription-billed drivers (Grok/Antigravity)
                // are excluded by driver without setting `estimated_usd_partial`,
                // so a measured cell with no estimate and no partial flag is
                // subscription-unpriced rather than a pricing gap.
                if cell.measurement.runsMeasured > 0 && !cell.measurement.estimatedUsdPartial {
                    return .excluded
                }
                return .noData(unmeasured: cell.measurement, noun: "priced runs")
            }
            return cell.measurement.estimatedUsdPartial
                ? .floor("some runs could not be priced (gross input, unknown model or incomplete tokens)")
                : .complete
        }
    }

    // MARK: - Hover

    /// `date` is in chart space (what `chartXSelection` reports).
    func bucketIndex(nearest date: Date) -> Int? {
        guard !plotSpans.isEmpty else { return nil }
        if let inside = plotSpans.firstIndex(where: { date >= $0.start && date < $0.end }) {
            return inside
        }
        return plotSpans.indices.min { lhs, rhs in
            abs(plotSpans[lhs].mid.timeIntervalSince(date)) < abs(plotSpans[rhs].mid.timeIntervalSince(date))
        }
    }

    func title(forBucket index: Int) -> String {
        UsageSeriesGeometry.bucketLabel(spans[index].start, width: report.bucketWidth)
    }

    func hoverTokensByGroup(_ index: Int) -> UsageHoverReadout {
        readout(index) { cell in
            let label = groupLabel(cell.group)
            if cell.measurement.runsMeasured == 0 {
                return UsageHoverRow(label: label, value: "no token data", seriesKey: cell.group)
            }
            let floor = cell.measurement.runsMeasured < cell.measurement.runsTotal || cell.measurement.tokensPartial
            let text = TokenUsageFormatter.grouped(cell.measurement.totalTokens) + (floor ? " (floor)" : "")
            return UsageHoverRow(label: label, value: text, seriesKey: cell.group)
        }
    }

    func hoverTokensByClass(_ index: Int) -> UsageHoverReadout {
        guard let cell = report.buckets[index].cells.first else { return UsageHoverReadout(title: title(forBucket: index), rows: []) }
        if cell.measurement.runsMeasured == 0 {
            return UsageHoverReadout(
                title: title(forBucket: index),
                rows: [UsageHoverRow(label: "Tokens", value: "no token data", seriesKey: nil)]
            )
        }
        let rows = UsageTokenClass.allCases.map { tokenClass -> UsageHoverRow in
            let value = tokenClass.tokens(cell.measurement).map(TokenUsageFormatter.grouped) ?? "unknown"
            return UsageHoverRow(label: tokenClass.rawValue, value: value, seriesKey: tokenClass.rawValue)
        }
        return UsageHoverReadout(title: title(forBucket: index), rows: rows)
    }

    func hoverWork(_ index: Int, metric: UsageWorkMetric?) -> UsageHoverReadout {
        if let metric {
            return readout(index) { cell in
                let label = groupLabel(cell.group)
                return UsageHoverRow(label: label, value: "\(metric.count(cell))", seriesKey: cell.group)
            }
        }
        guard let cell = report.buckets[index].cells.first else { return UsageHoverReadout(title: title(forBucket: index), rows: []) }
        return UsageHoverReadout(
            title: title(forBucket: index),
            rows: UsageWorkMetric.allCases.map {
                UsageHoverRow(label: $0.title, value: "\($0.count(cell))", seriesKey: $0.title)
            }
        )
    }

    func hoverNormalized(_ index: Int, normalizer: UsageNormalizer) -> UsageHoverReadout {
        readout(index) { cell in
            let label = groupLabel(cell.group)
            let value = normalizer.value(cell.ratios).map { UsageSeriesNumber.compact($0) + " " + normalizer.unitLabel }
            return UsageHoverRow(label: label, value: value ?? "no value", seriesKey: cell.group)
        }
    }

    func hoverMix(_ index: Int) -> UsageHoverReadout {
        readout(index) { cell in
            let label = groupLabel(cell.group)
            return UsageHoverRow(
                label: label,
                value: cell.shareOfBucketTotalTokens.map(UsageSeriesNumber.percent) ?? "no measured runs",
                seriesKey: cell.group
            )
        }
    }

    func hoverUsd(_ index: Int) -> UsageHoverReadout {
        readout(index) { cell in
            let label = groupLabel(cell.group)
            let value: String
            if let usd = cell.measurement.estimatedUsd {
                value = TokenUsageFormatter.usdAmount(usd) + (cell.measurement.estimatedUsdPartial ? " (partial)" : "")
            } else if cell.measurement.runsMeasured > 0 && !cell.measurement.estimatedUsdPartial {
                value = "unpriced (subscription)"
            } else {
                value = "not priced"
            }
            return UsageHoverRow(label: label, value: value, seriesKey: cell.group)
        }
    }

    // MARK: - Helpers

    private func readout(_ index: Int, _ row: (UsageCell) -> UsageHoverRow) -> UsageHoverReadout {
        UsageHoverReadout(title: title(forBucket: index), rows: report.buckets[index].cells.map(row))
    }

    private func bar(_ index: Int, seriesKey: String, series: String, value: Double) -> UsageBarPoint {
        UsageBarPoint(bucketIndex: index, seriesKey: seriesKey, series: series, x: plotSpans[index].start, value: value)
    }

    private enum CellVerdict {
        case complete
        /// Intentionally excluded without warning (e.g. subscription-billed
        /// drivers on the USD card). Neither counts as a drawn bar nor raises
        /// a gap/floor flag.
        case excluded
        case floor(String)
        case noData(unmeasured: CostMeasurement, noun: String = "token data")
    }

    /// One flag per bucket: `noData` when every cell lacks a figure, `floor`
    /// when some do and some do not (or a cell is itself a floor).
    private func flags(_ verdict: (UsageCell) -> CellVerdict) -> [UsageBucketFlag] {
        var flags: [UsageBucketFlag] = []
        for (index, bucket) in report.buckets.enumerated() {
            var noData: [String] = []
            var floors: [String] = []
            var completeCount = 0
            for cell in bucket.cells {
                let label = isGrouped ? "\(groupLabel(cell.group)): " : ""
                switch verdict(cell) {
                case .complete:
                    completeCount += 1
                case .excluded:
                    break
                case .floor(let reason):
                    floors.append(label + reason)
                case .noData(let measurement, let noun):
                    let runs = measurement.runsTotal
                    noData.append(label + "no \(noun) (\(runs) run\(runs == 1 ? "" : "s"), unknown — not zero)")
                }
            }
            if !noData.isEmpty, completeCount == 0, floors.isEmpty {
                flags.append(UsageBucketFlag(
                    bucketIndex: index,
                    kind: .noData,
                    at: plotSpans[index].mid,
                    detail: noData.joined(separator: "; ")
                ))
            } else if !noData.isEmpty || !floors.isEmpty {
                flags.append(UsageBucketFlag(
                    bucketIndex: index,
                    kind: .floor,
                    at: plotSpans[index].mid,
                    detail: (floors + noData).joined(separator: "; ")
                ))
            }
        }
        return flags
    }
}
