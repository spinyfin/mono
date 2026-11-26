import Charts
import SwiftUI

/// Which card the pointer is over. The rule line follows the pointer on every
/// card; only this card shows its values.
enum TokenUsageChartCard: String, Sendable {
    case tokens, work, normalized, mix, usd
}

struct TokenUsageHover: Equatable, Sendable {
    var bucketIndex: Int
    var card: TokenUsageChartCard
}

/// Initial picker state for the Over time tab. Capture scenes use it to
/// show a specific card configuration; the window uses the defaults.
struct TokenUsageOverTimeInitial: Equatable, Sendable {
    var normalizer: UsageNormalizer = .perRun
    var workMetric: UsageWorkMetric?
    var tokensByClass = true
    var hover: TokenUsageHover?
}

private struct UsageChartSeriesItem: Equatable, Sendable {
    let key: String
    let label: String
}

private enum UsageChartPalette {
    static let categorical: [Color] = [
        Color(red: 0.00, green: 0.45, blue: 0.70),
        Color(red: 0.90, green: 0.62, blue: 0.00),
        Color(red: 0.00, green: 0.62, blue: 0.45),
        Color(red: 0.84, green: 0.37, blue: 0.00),
        Color(red: 0.34, green: 0.71, blue: 0.91),
        Color(red: 0.80, green: 0.47, blue: 0.65),
        Color(red: 0.62, green: 0.58, blue: 0.00),
        Color(red: 0.45, green: 0.45, blue: 0.45),
    ]

    static func color(at index: Int) -> Color {
        categorical[index % categorical.count]
    }

    static let shade = Color.secondary.opacity(0.16)
    static let flag = Color.orange

    static func range(for domain: [String], ordered groupKeys: [String]) -> [Color] {
        domain.map { key in
            if let index = groupKeys.firstIndex(of: key) { return color(at: index) }
            if let index = UsageTokenClass.allCases.firstIndex(where: { $0.rawValue == key }) { return color(at: index) }
            if let index = UsageWorkMetric.allCases.firstIndex(where: { $0.title == key }) { return color(at: index) }
            return Color.gray
        }
    }
}

/// The Over time tab: five chart cards over one `TokenUsageSeriesReport`.
/// It draws what the engine computed; see `UsageSeriesMapper`.
struct TokenUsageOverTimeView: View {
    let report: TokenUsageSeriesReport?
    let loading: Bool
    let estimateUsd: Bool
    let connected: Bool

    @State private var tokensByClass: Bool
    @State private var workMetric: UsageWorkMetric?
    @State private var normalizer: UsageNormalizer
    @State private var hover: TokenUsageHover?

    init(
        report: TokenUsageSeriesReport?,
        loading: Bool,
        estimateUsd: Bool,
        connected: Bool,
        initial: TokenUsageOverTimeInitial = TokenUsageOverTimeInitial()
    ) {
        self.report = report
        self.loading = loading
        self.estimateUsd = estimateUsd
        self.connected = connected
        _tokensByClass = State(initialValue: initial.tokensByClass)
        _workMetric = State(initialValue: initial.workMetric)
        _normalizer = State(initialValue: initial.normalizer)
        _hover = State(initialValue: initial.hover)
    }

    var body: some View {
        if let report {
            let mapper = UsageSeriesMapper(report: report)
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    if report.spansCaptureBoundary {
                        captureBanner(report)
                    }
                    if mapper.isEmpty {
                        Text("No runs, executions or completed tasks in this range.")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, minHeight: 160)
                    } else {
                        LazyVGrid(
                            columns: [GridItem(.flexible(), spacing: 12), GridItem(.flexible(), spacing: 12)],
                            alignment: .leading,
                            spacing: 12
                        ) {
                            tokensCard(mapper)
                            workCard(mapper)
                            normalizedCard(mapper)
                            mixCard(mapper)
                            if estimateUsd {
                                usdCard(mapper)
                            }
                        }
                    }
                    notes(report, mapper: mapper)
                }
                .padding(14)
            }
            .accessibilityIdentifier("token-usage-over-time")
        } else if loading {
            ProgressView("Loading usage over time…")
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            Text(connected ? "No usage loaded yet." : "Waiting for engine connection…")
                .font(.callout)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    // MARK: - Cards

    private func tokensCard(_ mapper: UsageSeriesMapper) -> some View {
        let byClass = tokensByClass && !mapper.isGrouped
        let bars = byClass ? mapper.tokenBarsByClass() : mapper.tokenBarsByGroup()
        let flags = mapper.tokenFlags(byClass: byClass)
        return UsageChartCard(
            title: byClass ? "Tokens by class" : "Tokens by group",
            subtitle: "Measured tokens of runs created in each bucket"
        ) {
            if mapper.isGrouped {
                Text("Grouped").font(.caption).foregroundStyle(.secondary)
            } else {
                Picker("Tokens", selection: $tokensByClass) {
                    Text("By class").tag(true)
                    Text("By group").tag(false)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .frame(width: 160)
            }
        } content: {
            barChart(
                mapper,
                card: .tokens,
                bars: bars,
                flags: flags,
                includeCompletedTaskShade: false,
                emptyMessage: "No bucket in this range has token data. Unmeasured buckets are unknown, not zero.",
                readout: { index in
                    byClass ? mapper.hoverTokensByClass(index) : mapper.hoverTokensByGroup(index)
                }
            )
        }
    }

    private func workCard(_ mapper: UsageSeriesMapper) -> some View {
        let effective = mapper.isGrouped ? (workMetric ?? .runs) : workMetric
        let bars = mapper.workBars(metric: effective)
        return UsageChartCard(
            title: "Work volume",
            subtitle: effective == nil
                ? "Runs, completed executions, completed tasks and distinct PRs"
                : "\(effective?.title ?? "") by group"
        ) {
            Picker("Measure", selection: Binding(get: { effective }, set: { workMetric = $0 })) {
                if !mapper.isGrouped {
                    Text("All").tag(Optional<UsageWorkMetric>.none)
                }
                ForEach(UsageWorkMetric.allCases) { metric in
                    Text(metric.title).tag(Optional(metric))
                }
            }
            .labelsHidden()
            .frame(width: 190)
        } content: {
            barChart(
                mapper,
                card: .work,
                bars: bars,
                flags: [],
                includeCompletedTaskShade: effective == nil || effective == .tasksCompleted,
                emptyMessage: "No work in this range.",
                sideBySide: effective == nil,
                readout: { mapper.hoverWork($0, metric: effective) }
            )
            if effective == .distinctPrs && mapper.isGrouped {
                Text("A PR whose executions span groups is counted in each group.")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func normalizedCard(_ mapper: UsageSeriesMapper) -> some View {
        let segments = mapper.normalizedSegments(normalizer)
        let shades = mapper.shades(includeCompletedTask: normalizer == .perCompletedTask)
        return UsageChartCard(
            title: "Normalized: \(normalizer.unitLabel)",
            subtitle: mapper.normalizerCaption(normalizer)
        ) {
            Picker("Normalizer", selection: $normalizer) {
                ForEach(UsageNormalizer.allCases) { each in
                    Text(each.title).tag(each)
                }
            }
            .labelsHidden()
            .frame(width: 210)
        } content: {
            if segments.isEmpty {
                emptyCard("No bucket has a value for this normalizer. A missing value is a gap, not zero.")
            } else {
                let seriesItems = uniqueSeries(segments.map { UsageChartSeriesItem(key: $0.seriesKey, label: $0.series) })
                Chart {
                    shadeMarks(shades)
                    ForEach(segments) { segment in
                        ForEach(Array(segment.points.enumerated()), id: \.offset) { _, point in
                            LineMark(
                                x: .value("Bucket", point.x),
                                y: .value(normalizer.unitLabel, point.y),
                                series: .value("Segment", segment.id)
                            )
                            .foregroundStyle(by: .value("Group", segment.seriesKey))
                            .interpolationMethod(.linear)
                            .lineStyle(StrokeStyle(lineWidth: 2))
                            PointMark(x: .value("Bucket", point.x), y: .value(normalizer.unitLabel, point.y))
                                .foregroundStyle(by: .value("Group", segment.seriesKey))
                                .symbolSize(28)
                        }
                    }
                    hoverRule(mapper, card: .normalized) { mapper.hoverNormalized($0, normalizer: normalizer) }
                }
                .modifier(UsageChartStyle(
                    mapper: mapper,
                    series: seriesItems,
                    card: .normalized,
                    hover: $hover
                ))
                shadeCaption(shades)
            }
        }
    }

    private func mixCard(_ mapper: UsageSeriesMapper) -> some View {
        let bars = mapper.mixBars()
        return UsageChartCard(
            title: "Mix",
            subtitle: "Each group's share of the bucket's measured tokens"
        ) {
            EmptyView()
        } content: {
            if !mapper.isGrouped {
                emptyCard("Group by driver, model, effort or another dimension to see the mix.")
            } else {
                barChart(
                    mapper,
                    card: .mix,
                    bars: bars,
                    flags: mapper.mixFlags(),
                    includeCompletedTaskShade: false,
                    emptyMessage: "No bucket has a measured share.",
                    percentAxis: true,
                    readout: { mapper.hoverMix($0) }
                )
            }
        }
    }

    private func usdCard(_ mapper: UsageSeriesMapper) -> some View {
        UsageChartCard(
            title: "Estimated USD",
            subtitle: "ESTIMATE, not billing truth. Subscription drivers and unpriceable runs are not priced."
        ) {
            EmptyView()
        } content: {
            barChart(
                mapper,
                card: .usd,
                bars: mapper.usdBars(),
                flags: mapper.usdFlags(),
                includeCompletedTaskShade: false,
                emptyMessage: "No bucket has a priced run.",
                dollarAxis: true,
                readout: { mapper.hoverUsd($0) }
            )
        }
    }

    // MARK: - Chart builders

    @ViewBuilder
    private func barChart(
        _ mapper: UsageSeriesMapper,
        card: TokenUsageChartCard,
        bars: [UsageBarPoint],
        flags: [UsageBucketFlag],
        includeCompletedTaskShade: Bool,
        emptyMessage: String,
        sideBySide: Bool = false,
        percentAxis: Bool = false,
        dollarAxis: Bool = false,
        readout: @escaping (Int) -> UsageHoverReadout
    ) -> some View {
        let shades = mapper.shades(includeCompletedTask: includeCompletedTaskShade)
        if bars.isEmpty {
            emptyCard(flags.isEmpty ? emptyMessage : "\(emptyMessage) \(flags.count) bucket\(flags.count == 1 ? "" : "s") flagged.")
        } else {
            let seriesItems = uniqueSeries(bars.map { UsageChartSeriesItem(key: $0.seriesKey, label: $0.series) })
            Chart {
                shadeMarks(shades)
                ForEach(bars) { bar in
                    BarMark(
                        x: .value("Bucket", bar.x, unit: UsageSeriesGeometry.unit(mapper.report.bucketWidth)),
                        y: .value("Value", bar.value)
                    )
                    .foregroundStyle(by: .value("Series", bar.seriesKey))
                    .position(by: .value("Series", sideBySide ? bar.seriesKey : ""))
                }
                ForEach(flags) { flag in
                    PointMark(x: .value("Bucket", flag.at), y: .value("Unknown", 0.0))
                        .symbol(flag.kind == .noData ? .cross : .diamond)
                        .symbolSize(70)
                        .foregroundStyle(UsageChartPalette.flag)
                }
                hoverRule(mapper, card: card, readout: readout)
            }
            .modifier(UsageChartStyle(
                mapper: mapper,
                series: seriesItems,
                card: card,
                hover: $hover,
                percentAxis: percentAxis,
                dollarAxis: dollarAxis
            ))
            flagLegend(flags)
            shadeCaption(shades)
        }
    }

    @ChartContentBuilder
    private func shadeMarks(_ shades: [UsageShade]) -> some ChartContent {
        ForEach(shades) { shade in
            RectangleMark(xStart: .value("From", shade.from), xEnd: .value("To", shade.to))
                .foregroundStyle(UsageChartPalette.shade)
        }
    }

    @ChartContentBuilder
    private func hoverRule(
        _ mapper: UsageSeriesMapper,
        card: TokenUsageChartCard,
        readout: @escaping (Int) -> UsageHoverReadout
    ) -> some ChartContent {
        if let hover, hover.bucketIndex < mapper.plotSpans.count {
            RuleMark(x: .value("Hover", mapper.plotSpans[hover.bucketIndex].mid))
                .foregroundStyle(Color.secondary.opacity(0.6))
                .lineStyle(StrokeStyle(lineWidth: 1, dash: [3, 3]))
                .annotation(
                    position: .overlay,
                    alignment: .top,
                    spacing: 2,
                    overflowResolution: .init(x: .fit(to: .chart), y: .fit(to: .chart))
                ) {
                    if hover.card == card {
                        UsageReadoutView(readout: readout(hover.bucketIndex), groupKeys: mapper.report.groups)
                    }
                }
        }
    }

    private func uniqueSeries(_ items: [UsageChartSeriesItem]) -> [UsageChartSeriesItem] {
        var seen = Set<String>()
        return items.filter { seen.insert($0.key).inserted }
    }

    private func emptyCard(_ message: String) -> some View {
        Text(message)
            .font(.callout)
            .foregroundStyle(.secondary)
            .multilineTextAlignment(.center)
            .frame(maxWidth: .infinity, minHeight: 180)
    }

    @ViewBuilder
    private func flagLegend(_ flags: [UsageBucketFlag]) -> some View {
        if !flags.isEmpty {
            HStack(spacing: 12) {
                if flags.contains(where: { $0.kind == .noData }) {
                    Label("no data (unknown, not zero)", systemImage: "xmark")
                }
                if flags.contains(where: { $0.kind == .floor }) {
                    Label("floor (unmeasured runs or unreported class)", systemImage: "diamond.fill")
                }
            }
            .font(.caption2)
            .foregroundStyle(UsageChartPalette.flag)
        }
    }

    @ViewBuilder
    private func shadeCaption(_ shades: [UsageShade]) -> some View {
        if !shades.isEmpty {
            HStack(spacing: 6) {
                Rectangle()
                    .fill(UsageChartPalette.shade)
                    .overlay(Rectangle().stroke(Color.secondary.opacity(0.4), lineWidth: 0.5))
                    .frame(width: 14, height: 9)
                Text(shades.map(\.label).joined(separator: " · "))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
        }
    }

    // MARK: - Banner and notes

    private func captureBanner(_ report: TokenUsageSeriesReport) -> some View {
        let date = TokenUsageFormatter.utcDateString(epochS: report.captureStartEpochS)
        return Text("Totals are a floor: runs before \(date) have no token data. That region is shaded and shows no line or bar.")
            .font(.caption)
            .foregroundStyle(.orange)
    }

    private func notes(_ report: TokenUsageSeriesReport, mapper: UsageSeriesMapper) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text("\(report.bucketWidth.label.capitalized) buckets (UTC), chosen by the engine from the range.")
                .font(.caption)
                .foregroundStyle(.secondary)
            if let gross = report.coverage.byCaveat.first(where: { $0.code == "gross_input" }), gross.runs > 0 {
                Text("\(gross.runs) run\(gross.runs == 1 ? "" : "s") report gross input (Codex includes cache reads in input), so token figures across drivers are not strictly comparable.")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
            ForEach(Array(mapper.noteDetails.enumerated()), id: \.offset) { _, detail in
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }
}

// MARK: - Chart scaffolding

private struct UsageChartCard<Accessory: View, Content: View>: View {
    let title: String
    let subtitle: String
    @ViewBuilder var accessory: () -> Accessory
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline) {
                Text(title).font(.headline)
                Spacer()
                accessory()
            }
            .frame(minHeight: 26)
            Text(subtitle)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(3)
                .fixedSize(horizontal: false, vertical: true)
            content()
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .topLeading)
        .background(Color(nsColor: .controlBackgroundColor), in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(Color.secondary.opacity(0.2)))
    }
}

/// Axes, colour scale, domain and hover selection shared by every card.
private struct UsageChartStyle: ViewModifier {
    let mapper: UsageSeriesMapper
    let series: [UsageChartSeriesItem]
    let card: TokenUsageChartCard
    @Binding var hover: TokenUsageHover?
    var percentAxis = false
    var dollarAxis = false

    func body(content: Content) -> some View {
        let domain = mapper.domain
        let keys = series.map(\.key)
        let colors = UsageChartPalette.range(for: keys, ordered: mapper.report.groups)
        let base = content
            .chartForegroundStyleScale(
                domain: keys,
                range: colors
            )
            .chartXScale(domain: domain ?? Date.distantPast...Date.distantFuture)
            .chartXAxis {
                AxisMarks(values: .automatic(desiredCount: 6)) { _ in
                    AxisGridLine()
                    AxisTick()
                    AxisValueLabel(format: xFormat)
                }
            }
            .chartYAxis {
                AxisMarks { value in
                    AxisGridLine()
                    AxisValueLabel {
                        if let number = value.as(Double.self) {
                            Text(yLabel(number))
                        }
                    }
                }
            }
            .chartLegend(position: .bottom, alignment: .leading, spacing: 6) {
                UsageChartLegendView(series: series, colors: colors)
            }
            .chartXSelection(value: Binding<Date?>(
                get: { hover.flatMap { $0.bucketIndex < mapper.plotSpans.count ? mapper.plotSpans[$0.bucketIndex].mid : nil } },
                set: { date in
                    if let date, let index = mapper.bucketIndex(nearest: date) {
                        hover = TokenUsageHover(bucketIndex: index, card: card)
                    } else if hover?.card == card {
                        hover = nil
                    }
                }
            ))
            .environment(
                \.calendar,
                UsageSeriesGeometry.chartCalendar(mapper.report.bucketWidth, base: mapper.plotCalendar)
            )
            .frame(height: 200)
        if percentAxis {
            base.chartYScale(domain: 0.0...1.0)
        } else {
            base
        }
    }

    private var xFormat: Date.FormatStyle {
        let base = Date.FormatStyle(timeZone: mapper.plotCalendar.timeZone)
        switch mapper.report.bucketWidth {
        case .hour: return base.day().hour()
        case .day, .week: return base.month(.abbreviated).day()
        case .month: return base.month(.abbreviated).year(.twoDigits)
        }
    }

    private func yLabel(_ number: Double) -> String {
        if percentAxis { return UsageSeriesNumber.percent(number) }
        if dollarAxis { return "$" + UsageSeriesNumber.compact(number) }
        return UsageSeriesNumber.compact(number)
    }
}

private struct UsageChartLegendView: View {
    let series: [UsageChartSeriesItem]
    let colors: [Color]

    var body: some View {
        FlowLayout(horizontalSpacing: 10, verticalSpacing: 4) {
            ForEach(Array(series.enumerated()), id: \.element.key) { index, item in
                HStack(spacing: 4) {
                    Circle()
                        .fill(index < colors.count ? colors[index] : Color.gray)
                        .frame(width: 7, height: 7)
                    Text(item.label)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
        }
    }
}

private struct UsageReadoutView: View {
    let readout: UsageHoverReadout
    let groupKeys: [String]

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(readout.title)
                .font(.caption.weight(.semibold))
            ForEach(Array(readout.rows.enumerated()), id: \.offset) { _, row in
                HStack(spacing: 6) {
                    if let seriesKey = row.seriesKey {
                        Circle()
                            .fill(UsageChartPalette.range(for: [seriesKey], ordered: groupKeys)[0])
                            .frame(width: 7, height: 7)
                    }
                    Text(row.label)
                    Spacer(minLength: 8)
                    Text(row.value).monospacedDigit()
                }
                .font(.caption2)
            }
        }
        .padding(6)
        .frame(minWidth: 150, maxWidth: 260)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 6))
        .overlay(RoundedRectangle(cornerRadius: 6).stroke(Color.secondary.opacity(0.3)))
    }
}
