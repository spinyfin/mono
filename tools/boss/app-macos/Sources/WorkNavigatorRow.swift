import SwiftUI

struct NavigatorRowPresentation {
    let row: NavigatorRow
    let project: String?
    let width: CGFloat

    var isWorkerActive = false

    /// True when any of the row's activities maps (by run id, falling back
    /// to execution id) to a live worker that is currently working.
    static func isWorkerActive(row: NavigatorRow, liveStates: [String: WorkerLiveState]) -> Bool {
        row.projection.activities.contains { activity in
            liveStates[activity.runId ?? activity.executionId]?.activity == .working
        }
    }

    var showsSecondaryIndicators: Bool { width >= 340 }
    var health: (label: String, symbol: String)? {
        if row.task.prMergeableState == "conflicting" {
            return row.task.conflictResolutionDeferred
                ? ("Conflict — resolves on merge", "info.circle")
                : ("Merge conflict", "arrow.triangle.branch")
        }
        if row.task.ciRequiredState == "fail" { return ("CI failed", "xmark.circle") }
        if row.task.reviewRequiredState == "changes_requested" { return ("Review changes requested", "text.bubble") }
        if row.task.ciRequiredState == "in_progress" { return ("CI pending", "clock") }
        if row.task.reviewRequiredState == "approved" { return ("Review approved", "checkmark.circle") }
        if row.task.ciRequiredState == "success" { return ("CI passed", "checkmark.circle") }
        return nil
    }
    var healthFacts: [String] {
        [row.task.prMergeableState == "conflicting"
            ? (row.task.conflictResolutionDeferred ? "Conflict resolves on merge" : "Merge conflict") : nil,
         row.task.ciRequiredState.map { "CI \($0.replacingOccurrences(of: "_", with: " "))" },
         row.task.reviewRequiredState.map { "Review \($0.replacingOccurrences(of: "_", with: " "))" }]
            .compactMap { $0 }
    }
    var symbol: String {
        switch row.task.kind {
        case "chore": "wrench.and.screwdriver"
        case "followup": "arrow.turn.down.right"
        case "investigation": "magnifyingglass"
        case "revision": "arrow.triangle.branch"
        case "design": "pencil.and.outline"
        case "task", "project_task": "checkmark.circle"
        default: "questionmark.circle"
        }
    }
    var accessibilityLabel: String {
        ([row.projection.knownSection?.title, row.task.kindLabel, row.task.name,
         row.projection.reasonLabel, project,
         row.projection.knownSection == .working ? (isWorkerActive ? "Worker active" : "Worker idle") : nil,
         "\(row.projection.questionCount) questions or input requests",
         "\(row.projection.revisions.open) open revisions"].compactMap { $0 } + healthFacts)
            .joined(separator: ", ")
    }
}

struct WorkNavigatorRow: View {
    let row: NavigatorRow
    let project: String?
    let width: CGFloat
    let isSelected: Bool
    let isWorkerActive: Bool
    @Environment(\.dynamicTypeSize) private var textSize
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let presentation = NavigatorRowPresentation(row: row, project: project, width: width, isWorkerActive: isWorkerActive)
        HStack(spacing: 6) {
            if row.projection.knownSection == .working {
                Image(systemName: "circle.fill").font(.system(size: 5))
                    .foregroundStyle(isWorkerActive ? Color.green : Color(nsColor: .tertiaryLabelColor))
                    .symbolEffect(.pulse, options: .repeating, isActive: isWorkerActive && !reduceMotion)
            }
            Image(systemName: presentation.symbol).frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text(row.task.name).font(.system(size: textSize.isAccessibilitySize ? 17 : 13)).lineLimit(1)
                if textSize.isAccessibilitySize {
                    Text(row.projection.reasonLabel).font(.system(size: 13)).foregroundStyle(.secondary).lineLimit(1)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .layoutPriority(1)
            if presentation.showsSecondaryIndicators, let project {
                Text(project).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    .frame(maxWidth: 80, alignment: .trailing)
            }
            if presentation.showsSecondaryIndicators, row.projection.revisions.open > 0 {
                Text("↻ \(row.projection.revisions.open)").font(.caption).fixedSize()
            }
            if let health = presentation.health {
                Image(systemName: health.symbol).help(presentation.healthFacts.joined(separator: ", "))
            }
            if row.projection.questionCount > 0 {
                Label("\(row.projection.questionCount)", systemImage: "questionmark.bubble")
                    .font(.caption).fixedSize()
            }
        }
        .padding(.horizontal, 8)
        .frame(maxWidth: .infinity, minHeight: textSize.isAccessibilitySize ? 52 : 30, alignment: .leading)
        .background(isSelected ? Color.accentColor.opacity(0.18) : Color.clear)
        .clipShape(RoundedRectangle(cornerRadius: 5))
        .contentShape(Rectangle())
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(presentation.accessibilityLabel)
        .accessibilityAddTraits(isSelected ? [.isSelected] : [])
        .help(presentation.accessibilityLabel)
    }
}

/// Mounts a row with its own live-state value; the surrounding list never
/// observes live worker traffic.
struct WorkNavigatorObservedRow: View {
    let row: NavigatorRow
    let project: String?
    let width: CGFloat
    let isSelected: Bool
    @StateObject private var live: NavigatorRowLiveState

    init(row: NavigatorRow, project: String?, width: CGFloat, isSelected: Bool, model: ChatViewModel) {
        self.row = row
        self.project = project
        self.width = width
        self.isSelected = isSelected
        _live = StateObject(wrappedValue: NavigatorRowLiveState(model: model, row: row))
    }

    var body: some View {
        WorkNavigatorRow(row: row, project: project, width: width, isSelected: isSelected,
                         isWorkerActive: live.isWorkerActive)
            .onChange(of: row) { _, row in live.setRow(row) }
    }
}
