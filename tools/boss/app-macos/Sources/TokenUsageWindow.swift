import AppKit
import Combine
import SwiftUI

enum TokenUsageRangePreset: String, CaseIterable, Identifiable {
    case h24, d7, d30, d90, all, custom
    var id: String { rawValue }
    var title: String {
        switch self {
        case .h24: return "24h"
        case .d7: return "7d"
        case .d30: return "30d"
        case .d90: return "90d"
        case .all: return "All"
        case .custom: return "Custom"
        }
    }
}

enum TokenUsageCaptureScene: String, Equatable, Sendable {
    case tree
    case grouped
    case captureBoundary = "capture-boundary"
    case inspector
}

/// Window → Token Usage. Thin client of `get_token_usage_rows`: the engine
/// owns every total, and this view only lays out, filters and formats.
struct TokenUsageWindow: View {
    @EnvironmentObject private var chatModel: ChatViewModel
    @AppStorage("boss.tokenUsage.visible", store: BossDefaults.store) private var isOpen = false

    @State private var range: TokenUsageRangePreset
    @State private var customSince: Date
    @State private var customUntil: Date
    @State private var groupPrimary: UsageDimension?
    @State private var groupSecondary: UsageDimension?
    @State private var rollUpRevisions: Bool
    @State private var estimateUsd: Bool
    @State private var selectedFilterValues: [UsageDimension: Set<String>] = [:]
    @State private var selection: Set<TokenUsageTableNode.ID> = []
    @State private var sortOrder: [KeyPathComparator<TokenUsageTableNode>] = [
        KeyPathComparator(\.sortName, order: .forward),
    ]
    @State private var roots: [TokenUsageTableNode] = []
    @State private var debounceTask: Task<Void, Never>?
    @State private var hasAppeared = false

    private let captureScene: TokenUsageCaptureScene?
    private let pollTimer = Timer.publish(every: 60, on: .main, in: .common).autoconnect()

    init(captureScene: TokenUsageCaptureScene? = nil) {
        self.captureScene = captureScene
        let now = Date()
        _customUntil = State(initialValue: now)
        _customSince = State(initialValue: now.addingTimeInterval(-7 * 86_400))
        switch captureScene {
        case .captureBoundary:
            _range = State(initialValue: .all)
        default:
            _range = State(initialValue: .d7)
        }
        switch captureScene {
        case .grouped:
            _groupPrimary = State(initialValue: .driver)
            _groupSecondary = State(initialValue: .model)
        default:
            _groupPrimary = State(initialValue: nil)
            _groupSecondary = State(initialValue: nil)
        }
        _rollUpRevisions = State(initialValue: true)
        _estimateUsd = State(initialValue: captureScene == .inspector)
    }

    var body: some View {
        VStack(spacing: 0) {
            toolbar
            Divider()
            if let message = chatModel.tokenUsage.error {
                inlineBanner(message)
            }
            if TokenUsageTree.mixesInputConventions(roots) {
                mixedConventionNote
            }
            HSplitView {
                table
                    .opacity(chatModel.tokenUsage.error != nil && chatModel.tokenUsage.report != nil ? 0.55 : 1)
                if let node = selectedNode {
                    TokenUsageInspector(node: node, spansCaptureBoundary: spansCaptureBoundary) {
                        revealOnBoard(node)
                    }
                    .frame(minWidth: 280, idealWidth: 320, maxWidth: 420)
                }
            }
            Divider()
            footer
        }
        .frame(minWidth: 900, minHeight: 520)
        .onAppear {
            isOpen = true
            hasAppeared = true
            if chatModel.isConnected {
                chatModel.requestTokenUsage(makeQuery())
            }
        }
        .onDisappear {
            isOpen = false
            debounceTask?.cancel()
        }
        .onReceive(pollTimer) { _ in
            guard isOpen, chatModel.isConnected else { return }
            chatModel.requestTokenUsage(makeQuery())
        }
        .onChange(of: chatModel.isConnected) { _, connected in
            guard connected, isOpen else { return }
            chatModel.requestTokenUsage(makeQuery())
        }
        .onChange(of: queryFingerprint) { _, _ in
            guard hasAppeared else { return }
            debounceRefresh()
        }
        .onChange(of: chatModel.tokenUsage.report) { _, report in
            rebuildTree(from: report)
            applyCaptureSelection()
        }
        .onChange(of: sortOrder) { _, _ in
            roots = TokenUsageTree.sort(roots, by: sortOrder)
        }
        .background(TokenUsageOutlineExpander(expand: captureScene == .tree || captureScene == .inspector))
    }

    // MARK: - Toolbar

    private var toolbar: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                Text("Token Usage")
                    .font(.headline)
                NativeSegmentedPicker(
                    "Range",
                    selection: $range,
                    options: TokenUsageRangePreset.allCases,
                    title: \.title
                )
                .frame(width: 320)
                if range == .custom {
                    DatePicker("From", selection: $customSince, displayedComponents: [.date, .hourAndMinute])
                        .labelsHidden()
                    Text("–")
                        .foregroundStyle(.secondary)
                    DatePicker("To", selection: $customUntil, displayedComponents: [.date, .hourAndMinute])
                        .labelsHidden()
                }
                Spacer()
                Toggle("Roll up revisions", isOn: $rollUpRevisions)
                    .toggleStyle(.checkbox)
                Toggle("Estimated USD", isOn: $estimateUsd)
                    .toggleStyle(.checkbox)
                stalePill
                Button(action: { chatModel.requestTokenUsage(makeQuery()) }) {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                .disabled(!chatModel.isConnected)
                .help(chatModel.isConnected ? "Refresh token usage" : "Waiting for engine connection")
            }
            HStack(spacing: 8) {
                groupPicker("Group by", selection: $groupPrimary, excluding: groupSecondary)
                groupPicker("Then by", selection: $groupSecondary, excluding: groupPrimary)
                    .disabled(groupPrimary == nil)
                if groupPrimary != nil {
                    Button("Clear grouping") {
                        groupPrimary = nil
                        groupSecondary = nil
                    }
                    .buttonStyle(.borderless)
                }
                Spacer()
                if chatModel.tokenUsage.loading {
                    ProgressView()
                        .controlSize(.small)
                }
            }
            filterChips
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    private func groupPicker(_ title: String, selection: Binding<UsageDimension?>, excluding: UsageDimension?) -> some View {
        Picker(title, selection: selection) {
            Text("None").tag(Optional<UsageDimension>.none)
            ForEach(UsageDimension.allCases) { dimension in
                if dimension != excluding {
                    Text(dimension.pickerLabel).tag(Optional(dimension))
                }
            }
        }
        .frame(maxWidth: 220)
    }

    @ViewBuilder
    private var filterChips: some View {
        let dimensions = chatModel.tokenUsage.report?.dimensionValues ?? []
        if !dimensions.isEmpty {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(alignment: .top, spacing: 12) {
                    ForEach(dimensions) { dim in
                        VStack(alignment: .leading, spacing: 4) {
                            Text(dim.dimension.pickerLabel)
                                .font(.caption2.weight(.semibold))
                                .foregroundStyle(.secondary)
                            HStack(spacing: 4) {
                                ForEach(dim.values) { value in
                                    filterChip(dimension: dim.dimension, value: value)
                                }
                            }
                        }
                    }
                    if !selectedFilterValues.isEmpty {
                        Button("Clear filters") { selectedFilterValues = [:] }
                            .font(.caption)
                            .buttonStyle(.borderless)
                    }
                }
            }
        }
    }

    private func filterChip(dimension: UsageDimension, value: UsageDimensionValue) -> some View {
        let selected = selectedFilterValues[dimension, default: []].contains(value.value)
        return Button {
            var set = selectedFilterValues[dimension, default: []]
            if selected { set.remove(value.value) } else { set.insert(value.value) }
            if set.isEmpty {
                selectedFilterValues[dimension] = nil
            } else {
                selectedFilterValues[dimension] = set
            }
        } label: {
            Text("\(value.label) \(value.runs)")
                .font(.caption)
                .padding(.horizontal, 8)
                .padding(.vertical, 3)
                .background(selected ? Color.accentColor.opacity(0.22) : Color.secondary.opacity(0.12), in: Capsule())
        }
        .buttonStyle(.plain)
        .help("\(value.label) — \(value.runs) run\(value.runs == 1 ? "" : "s")")
    }

    private var stalePill: some View {
        Group {
            if let received = chatModel.tokenUsage.receivedAt {
                let age = Date().timeIntervalSince(received)
                Text(age >= 60 ? "stale" : "live")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(age >= 60 ? Color.orange : Color.secondary)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background((age >= 60 ? Color.orange : Color.secondary).opacity(0.12), in: Capsule())
                    .help("Last updated \(Int(age))s ago")
            }
        }
    }

    private func inlineBanner(_ message: String) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            Text(message)
                .font(.callout)
                .textSelection(.enabled)
            Spacer()
            Button("Dismiss") { chatModel.tokenUsage.error = nil }
                .buttonStyle(.borderless)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.orange.opacity(0.12))
        .accessibilityIdentifier("token-usage-error-banner")
    }

    private var mixedConventionNote: some View {
        Text("Totals mix net and gross input; Codex input already includes cache reads, so ranking by Total is not comparable across drivers.")
            .font(.caption)
            .foregroundStyle(.orange)
            .padding(.horizontal, 14)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Color.orange.opacity(0.08))
    }

    // MARK: - Table

    private var table: some View {
        // SwiftUI's TableColumnBuilder only accepts 10 columns per
        // buildBlock, so identity and token columns are two contents.
        Table(roots, children: \.children, selection: $selection, sortOrder: $sortOrder) {
            identityColumns
            tokenColumns
        }
    }

    @TableColumnBuilder<TokenUsageTableNode, KeyPathComparator<TokenUsageTableNode>>
    private var identityColumns: some TableColumnContent<TokenUsageTableNode, KeyPathComparator<TokenUsageTableNode>> {
        TableColumn("Name", value: \.sortName) { node in
            nameCell(node)
        }
        .width(min: 180, ideal: 260)
        TableColumn("Driver", value: \.sortDriver) { node in
            Text(node.kind == .tasklessHeader || node.kind == .tasklessKind || node.kind == .group ? "" : node.driver)
                .help(driverHelp(node))
        }
        .width(min: 80, ideal: 110)
        TableColumn("Model", value: \.sortModel) { node in
            modelCell(node)
        }
        .width(min: 100, ideal: 140)
        TableColumn("Effort", value: \.sortEffort) { node in
            Text(node.effort)
        }
        .width(min: 60, ideal: 70)
        TableColumn("Reasoning", value: \.sortReasoning) { node in
            Text(node.reasoning)
                .help(node.kind == .task || node.kind == .revision ? "Current setting" : "")
        }
        .width(min: 80, ideal: 90)
        TableColumn("Status", value: \.sortStatus) { node in
            Text(node.status)
        }
        .width(min: 70, ideal: 80)
        TableColumn("Runs", value: \.sortRuns) { node in
            if node.kind == .tasklessHeader || node.kind == .tasklessKind {
                EmptyView()
            } else {
                Text(TokenUsageFormatter.runsRatio(
                    measured: node.runsMeasured,
                    total: node.runsTotal,
                    outsideWindow: node.runsOutsideWindow
                ))
                .monospacedDigit()
            }
        }
        .width(min: 70, ideal: 90)
        TableColumn("Input", value: \.sortInput) { node in
            tokenCell(node.inputTokens, node: node)
        }
        .width(min: 70, ideal: 80)
    }

    @TableColumnBuilder<TokenUsageTableNode, KeyPathComparator<TokenUsageTableNode>>
    private var tokenColumns: some TableColumnContent<TokenUsageTableNode, KeyPathComparator<TokenUsageTableNode>> {
        TableColumn("Output", value: \.sortOutput) { node in
            tokenCell(node.outputTokens, node: node)
        }
        .width(min: 70, ideal: 80)
        TableColumn("Cache write", value: \.sortCacheWrite) { node in
            tokenCell(node.cacheCreationTokens, node: node)
                .help(cacheWriteHelp(node))
        }
        .width(min: 80, ideal: 90)
        TableColumn("Cache read", value: \.sortCacheRead) { node in
            tokenCell(node.cacheReadTokens, node: node)
        }
        .width(min: 80, ideal: 90)
        TableColumn("Total", value: \.sortTotal) { node in
            totalCell(node)
        }
        .width(min: 90, ideal: 110)
        TableColumn("Active", value: \.sortActive) { node in
            Text(node.kind == .tasklessHeader || node.kind == .tasklessKind ? "" : TokenUsageFormatter.duration(ms: node.agentActiveMs))
                .monospacedDigit()
        }
        .width(min: 60, ideal: 70)
        TableColumn("Est. USD", value: \.sortUsd) { node in
            if estimateUsd {
                usdCell(node)
            } else {
                EmptyView()
            }
        }
        .width(min: estimateUsd ? 90 : 1, ideal: estimateUsd ? 110 : 1)
        TableColumn("Coverage", value: \.sortCoverage) { node in
            Text(coverageText(node))
                .font(.caption)
        }
        .width(min: 80, ideal: 100)
        TableColumn("Caveats") { node in
            caveatsCell(node)
        }
        .width(min: 70, ideal: 90)
    }

    private func nameCell(_ node: TokenUsageTableNode) -> some View {
        HStack(spacing: 6) {
            Text(node.name)
                .font(node.kind == .tasklessHeader ? .headline : .body)
                .lineLimit(1)
            if node.kind == .task || node.kind == .revision {
                Text(node.kindLabel)
                    .font(.caption2)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(.quaternary, in: Capsule())
            }
        }
    }

    private func modelCell(_ node: TokenUsageTableNode) -> some View {
        HStack(spacing: 4) {
            Text(node.model)
                .lineLimit(1)
            if node.modelIsSynthetic {
                Image(systemName: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                    .help("Observed model is the literal <synthetic> marker. Launched: \(node.launchedModel ?? "none")")
            }
        }
        .help(modelHelp(node))
    }

    @ViewBuilder
    private func tokenCell(_ value: Int64?, node: TokenUsageTableNode) -> some View {
        if node.kind == .tasklessHeader || node.kind == .tasklessKind {
            EmptyView()
        } else if let coverage = node.coverage {
            let figure = TokenUsageFormatter.tokenClass(
                value,
                coverage: coverage,
                spansCaptureBoundary: spansCaptureBoundary
            )
            Text(figure.display)
                .foregroundStyle(figure.isUnknown ? .secondary : .primary)
                .monospacedDigit()
                .help(figure.annotation ?? "")
        } else if let measurement = node.measurement {
            let figure = TokenUsageFormatter.measurementClass(
                value,
                measurement: measurement,
                spansCaptureBoundary: spansCaptureBoundary,
                dominantCaveat: node.dominantCaveat
            )
            Text(figure.display)
                .foregroundStyle(figure.isUnknown ? .secondary : .primary)
                .monospacedDigit()
        } else {
            Text("")
        }
    }

    @ViewBuilder
    private func totalCell(_ node: TokenUsageTableNode) -> some View {
        if node.kind == .tasklessHeader || node.kind == .tasklessKind {
            EmptyView()
        } else if let coverage = node.coverage {
            Text(TokenUsageFormatter.runTotal(
                tokens: node.totalTokens,
                coverage: coverage,
                caveats: node.caveats,
                inputConvention: node.inputConvention ?? .unknown,
                spansCaptureBoundary: spansCaptureBoundary
            ).display)
            .monospacedDigit()
        } else if let measurement = node.measurement {
            Text(TokenUsageFormatter.measurementTotal(
                measurement,
                spansCaptureBoundary: spansCaptureBoundary,
                containsGross: node.containsGross,
                dominantCaveat: node.dominantCaveat
            ).display)
            .monospacedDigit()
        } else {
            Text("")
        }
    }

    @ViewBuilder
    private func usdCell(_ node: TokenUsageTableNode) -> some View {
        if let figure = TokenUsageFormatter.usd(
            estimated: node.estimatedUsd,
            partial: node.estimatedUsdPartial,
            unpriceableCause: node.unpriceableCause
        ) {
            Text(figure.display)
                .foregroundStyle(figure.isUnknown ? .secondary : .primary)
                .monospacedDigit()
        } else {
            Text("")
        }
    }

    private func coverageText(_ node: TokenUsageTableNode) -> String {
        if node.kind == .tasklessHeader || node.kind == .tasklessKind { return "" }
        if let coverage = node.coverage {
            return TokenUsageFormatter.runCoverageBadge(
                coverage,
                reason: TokenUsageFormatter.unmeasuredReason(caveats: node.caveats)
            )
        }
        if let measurement = node.measurement {
            let reason = measurement.runsUnmeasured == measurement.runsTotal && measurement.runsTotal > 0
                ? TokenUsageFormatter.noMeasuredReason(measurement, dominantCaveat: node.dominantCaveat)
                : node.usageUnavailable
            return TokenUsageFormatter.coverageBadge(
                measurement: measurement,
                reason: reason
            )
        }
        return ""
    }

    private func caveatsCell(_ node: TokenUsageTableNode) -> some View {
        let codes = Array(Set(node.caveats.map(\.code))).sorted()
        return HStack(spacing: 3) {
            ForEach(codes, id: \.self) { code in
                Image(systemName: caveatIcon(code))
                    .foregroundStyle(.orange)
                    .help(TokenUsageFormatter.caveatLabel(
                        code: code,
                        detail: node.caveats.first { $0.code == code }?.detail
                    ))
            }
        }
        .help(codes.map { TokenUsageFormatter.caveatLabel(code: $0, detail: nil) }.joined(separator: ", "))
    }

    private func caveatIcon(_ code: String) -> String {
        switch code {
        case "pre_capture": return "clock"
        case "gross_input": return "arrow.left.arrow.right"
        case "synthetic_model": return "exclamationmark.triangle.fill"
        case "tokens_partial": return "circle.lefthalf.filled"
        case "usage_unavailable": return "questionmark.circle"
        case "subagents_excluded": return "person.2.slash"
        case "estimate_only": return "dollarsign.circle"
        default: return "info.circle"
        }
    }

    // MARK: - Footer

    private var footer: some View {
        VStack(alignment: .leading, spacing: 4) {
            if let report = chatModel.tokenUsage.report {
                let overall = TokenUsageFormatter.measurementTotal(
                    report.overall,
                    spansCaptureBoundary: report.spansCaptureBoundary,
                    containsGross: TokenUsageTree.containsGrossInput(roots),
                    dominantCaveat: TokenUsageFormatter.dominantCaveatCode(from: report.coverage.byCaveat)
                )
                HStack(spacing: 12) {
                    Text("Overall (direct): \(overall.display)")
                        .font(.callout.monospacedDigit())
                    Text(TokenUsageFormatter.coverageSummary(report.coverage))
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                    Spacer()
                }
                if report.spansCaptureBoundary {
                    let floorDate = TokenUsageFormatter.utcDateString(epochS: report.captureStartEpochS)
                    Text("Totals are a floor: runs before \(floorDate) have no token data.")
                        .font(.caption)
                        .foregroundStyle(.orange)
                }
                if report.truncated {
                    Text("More than \(report.runCap) runs matched; only the most recent are shown. Narrow the range or add a filter.")
                        .font(.caption)
                        .foregroundStyle(.orange)
                }
                ForEach(TokenUsageFormatter.footerNotes(for: report), id: \.code) { note in
                    Text(note.detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            } else if chatModel.tokenUsage.loading {
                Text("Loading token usage…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else if !chatModel.isConnected {
                Text("Waiting for engine connection…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                Text("No usage loaded yet.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
    }

    // MARK: - Query / tree

    private var queryFingerprint: String {
        "\(range.rawValue)-\(customSince.timeIntervalSince1970)-\(customUntil.timeIntervalSince1970)-\(groupPrimary?.rawValue ?? "")-\(groupSecondary?.rawValue ?? "")-\(rollUpRevisions)-\(estimateUsd)-\(selectedFilterValues.map { "\($0.key.rawValue):\($0.value.sorted().joined(separator: ","))" }.sorted().joined(separator: "|"))"
    }

    private var spansCaptureBoundary: Bool {
        chatModel.tokenUsage.report?.spansCaptureBoundary ?? false
    }

    private var selectedNode: TokenUsageTableNode? {
        guard let id = selection.first else { return nil }
        return TokenUsageTree.node(id: id, in: roots)
    }

    private func makeQuery() -> TokenUsageQuery {
        let now = Date()
        let until: Date
        let since: Date
        switch range {
        case .h24:
            until = now; since = now.addingTimeInterval(-86_400)
        case .d7:
            until = now; since = now.addingTimeInterval(-7 * 86_400)
        case .d30:
            until = now; since = now.addingTimeInterval(-30 * 86_400)
        case .d90:
            until = now; since = now.addingTimeInterval(-90 * 86_400)
        case .all:
            until = now; since = Date(timeIntervalSince1970: 0)
        case .custom:
            until = customUntil; since = customSince
        }
        var groupBy: [UsageDimension] = []
        if let groupPrimary { groupBy.append(groupPrimary) }
        if groupPrimary != nil, let groupSecondary { groupBy.append(groupSecondary) }
        let filters: [UsageFilter] = selectedFilterValues.compactMap { dimension, values in
            values.isEmpty ? nil : UsageFilter(dimension: dimension, values: values.sorted())
        }
        return TokenUsageQuery(
            sinceEpochS: Int64(since.timeIntervalSince1970),
            untilEpochS: Int64(until.timeIntervalSince1970),
            filters: filters,
            groupBy: groupBy,
            rollUpRevisions: rollUpRevisions,
            estimateUsd: estimateUsd
        )
    }

    private func debounceRefresh() {
        chatModel.invalidatePendingTokenUsage()
        debounceTask?.cancel()
        debounceTask = Task { @MainActor in
            try? await Task.sleep(nanoseconds: 150_000_000)
            guard !Task.isCancelled else { return }
            chatModel.requestTokenUsage(makeQuery())
        }
    }

    private func rebuildTree(from report: TokenUsageRowsReport?) {
        guard let report else {
            roots = []
            return
        }
        roots = TokenUsageTree.sort(TokenUsageTree.build(report), by: sortOrder)
    }

    private func applyCaptureSelection() {
        guard captureScene == .inspector else { return }
        if let synthetic = TokenUsageTree.firstNode(in: roots, where: { $0.modelIsSynthetic && $0.kind == .run }) {
            selection = [synthetic.id]
        }
    }

    private func revealOnBoard(_ node: TokenUsageTableNode) {
        guard let taskId = node.taskId, let productId = node.productId else { return }
        _ = chatModel.revealWorkCard(taskId, productID: productId)
    }

    private func driverHelp(_ node: TokenUsageTableNode) -> String {
        switch node.driverSource {
        case .launchTuple: return "Recorded at spawn"
        case .resolved: return "Current routing; the launch tuple was empty"
        case .unknown: return "Not recorded"
        case nil: return ""
        }
    }

    private func modelHelp(_ node: TokenUsageTableNode) -> String {
        var parts: [String] = []
        if let launched = node.launchedModel { parts.append("Launched: \(launched)") }
        if let effort = node.launchedEffort { parts.append("Effort: \(effort)") }
        return parts.joined(separator: " · ")
    }

    private func cacheWriteHelp(_ node: TokenUsageTableNode) -> String {
        switch (node.cacheCreation5m, node.cacheCreation1h) {
        case (let five?, let hour?):
            return "5m \(TokenUsageFormatter.grouped(five)) · 1h \(TokenUsageFormatter.grouped(hour))"
        default:
            return ""
        }
    }
}

private struct TokenUsageInspector: View {
    let node: TokenUsageTableNode
    let spansCaptureBoundary: Bool
    let onReveal: () -> Void

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Text(inspectorTitle)
                    .font(.headline)
                switch node.kind {
                case .task, .revision:
                    taskInspector
                case .execution:
                    executionInspector
                case .run:
                    runInspector
                case .group:
                    if let measurement = node.measurement {
                        measurementBlock("Group", measurement)
                    }
                case .tasklessHeader, .tasklessKind:
                    Text("Executions with no task row (answer agent, review guide, automation triage). Totals live on each execution so this header does not add token counts.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private var inspectorTitle: String {
        switch node.kind {
        case .task: return node.name
        case .revision: return "Revision · \(node.name)"
        case .execution: return node.kindLabel
        case .run: return "Run \(node.name)"
        case .group: return node.name
        case .tasklessHeader: return TokenUsageTree.tasklessHeaderName
        case .tasklessKind: return node.name
        }
    }

    @ViewBuilder
    private var taskInspector: some View {
        if let direct = node.measurement {
            measurementBlock("Direct", direct)
        }
        if let inclusive = node.inclusive {
            measurementBlock("Inclusive (chain)", inclusive, emphasize: true)
        }
        if !node.bySpendClass.isEmpty {
            Text("Spend class")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            ForEach(node.bySpendClass, id: \.label) { bucket in
                HStack {
                    Text(bucket.label)
                    Spacer()
                    Text(TokenUsageFormatter.measurementTotal(bucket.measurement, spansCaptureBoundary: spansCaptureBoundary).display)
                        .monospacedDigit()
                }
            }
        }
        if node.runsOutsideWindow > 0 {
            Text("\(node.runsOutsideWindow) more runs outside this range")
                .font(.callout)
                .foregroundStyle(.orange)
        }
        if let url = node.prUrl, let parsed = URL(string: url) {
            Link("Pull request", destination: parsed)
        }
        if node.taskId != nil, node.productId != nil {
            Button("Reveal on board", action: onReveal)
        }
    }

    @ViewBuilder
    private var executionInspector: some View {
        labeled("Driver", node.driver)
        labeled("Launched model", node.launchedModel ?? "not recorded")
        labeled("Launched effort", node.launchedEffort ?? "not recorded")
        labeled("Host", node.hostId ?? "not recorded")
        labeled("Created", epoch(node.createdAtEpochS))
        labeled("Started", epoch(node.startedAtEpochS))
        labeled("Finished", epoch(node.finishedAtEpochS))
        if let url = node.prUrl, let parsed = URL(string: url) {
            Link("Pull request", destination: parsed)
        }
        if let measurement = node.measurement {
            measurementBlock("Tokens", measurement)
        }
    }

    @ViewBuilder
    private var runInspector: some View {
        labeled("Status", node.status)
        labeled("Model", node.model)
        labeled("Host", node.hostId ?? "not recorded")
        labeled("Rounds", node.rounds.map { "\($0)" } ?? "—")
        labeled("Active time", TokenUsageFormatter.duration(ms: node.agentActiveMs))
        labeled("Usage unavailable", node.usageUnavailable ?? "not recorded")
        if let tokens = node.tokens, let coverage = node.coverage {
            Text("Token classes")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            classRow("Input", tokens.input, coverage)
            classRow("Output", tokens.output, coverage)
            classRow("Cache write", tokens.cacheCreation, coverage)
            classRow("Cache write (5m)", tokens.cacheCreation5m, coverage)
            classRow("Cache write (1h)", tokens.cacheCreation1h, coverage)
            classRow("Cache read", tokens.cacheRead, coverage)
        }
        if !node.models.isEmpty {
            Text("Per model")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            ForEach(node.models, id: \.model) { model in
                Text(model.model)
                    .font(.callout.monospaced())
            }
        }
        if !node.caveats.isEmpty {
            Text("Caveats")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            ForEach(Array(node.caveats.enumerated()), id: \.offset) { _, caveat in
                Text(TokenUsageFormatter.caveatSentence(caveat))
                    .font(.callout)
            }
        }
        if let usd = estimateUsdFigure {
            usd
        }
    }

    private var estimateUsdFigure: Text? {
        TokenUsageFormatter.usd(
            estimated: node.estimatedUsd,
            partial: node.estimatedUsdPartial,
            unpriceableCause: node.unpriceableCause
        ).map { Text($0.display) }
    }

    private func classRow(_ title: String, _ value: Int64?, _ coverage: RunCoverage) -> some View {
        let figure = TokenUsageFormatter.tokenClass(
            value,
            coverage: coverage,
            spansCaptureBoundary: spansCaptureBoundary
        )
        return HStack {
            Text(title)
            Spacer()
            Text(figure.display)
                .foregroundStyle(figure.isUnknown ? .secondary : .primary)
                .monospacedDigit()
        }
    }

    private func measurementBlock(_ title: String, _ measurement: CostMeasurement, emphasize: Bool = false) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            Text(TokenUsageFormatter.measurementTotal(
                measurement,
                spansCaptureBoundary: spansCaptureBoundary,
                containsGross: node.containsGross,
                dominantCaveat: node.dominantCaveat
            ).display)
                .font(emphasize ? .title3.weight(.semibold) : .body)
                .monospacedDigit()
            Text(TokenUsageFormatter.runsRatio(measured: measurement.runsMeasured, total: measurement.runsTotal))
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    private func labeled(_ title: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            Text(value)
                .font(.callout)
                .textSelection(.enabled)
        }
    }

    private func epoch(_ value: Int64?) -> String {
        guard let value else { return "—" }
        return Date(timeIntervalSince1970: TimeInterval(value)).formatted(date: .abbreviated, time: .shortened)
    }
}

/// Walks the hosted Table's NSOutlineView after a capture scene loads so a
/// revision chain is actually expanded in the screenshot.
private struct TokenUsageOutlineExpander: NSViewRepresentable {
    var expand: Bool

    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        view.setFrameSize(.zero)
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        guard expand else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) {
            var window: NSWindow? = nsView.window
            if window == nil {
                window = NSApp.windows.first { $0.title.contains("Token Usage") }
            }
            guard let content = window?.contentView else { return }
            TokenUsageCapture.expandOutlines(in: content)
        }
    }
}
