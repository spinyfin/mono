import AppKit
import os.log
import SwiftUI
import UpdateCore

struct WorkCardPopoverView: View {
    @ObservedObject var model: ChatViewModel
    let task: WorkTask

    @Environment(\.openWindow) private var openWindow

    /// Drives presentation of the Repo Change… picker sheet. Bound to
    /// the popover so the sheet inherits the popover's window context;
    /// closing the sheet returns focus to the popover the user came
    /// from rather than dropping back to the kanban underneath.
    @State private var presentingRepoPicker: Bool = false

    /// Reveals never-started `abandoned` executions in the list below,
    /// which the Executions section collapses by default behind a
    /// disclosure since they dominate high-churn work items without
    /// being interesting to a human reviewing status.
    @State private var showAllAbandonedAttempts: Bool = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top, spacing: 12) {
                VStack(alignment: .leading, spacing: 6) {
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text(task.name)
                            .font(.title3.weight(.semibold))
                        if let id = task.shortID {
                            Text("T" + String(id))
                                .font(.system(.caption, design: .monospaced))
                                .foregroundStyle(.secondary)
                                .accessibilityLabel("T" + String(id))
                        }
                    }
                    Text(task.kindLabel)
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
                Spacer(minLength: 12)
                Button("Edit") {
                    model.selectWorkCard(task.id)
                    model.presentEditSelectedWorkItem()
                }
            }

            if !task.description.isEmpty {
                descriptionSummary
            }

            VStack(alignment: .leading, spacing: 10) {
                if let projectName = model.projectName(for: task.projectID) {
                    metadataRow("Project", value: projectName)
                }
                metadataRow(
                    "Status",
                    value: task.status.replacingOccurrences(of: "_", with: " ").capitalized
                )
                if task.status == "blocked", let reason = task.blockedReason {
                    metadataRow(
                        "Blocked reason",
                        value: reason.replacingOccurrences(of: "_", with: " ").capitalized
                    )
                    // Verbatim — no title-casing, no truncation — unlike the
                    // pill's `.help()` tooltip, this row is plain readable
                    // text: it's reachable by keyboard focus and read by
                    // VoiceOver without depending on a hover gesture, which
                    // `.help()` alone does not guarantee (see PR discussion).
                    if let detail = task.blockedDetail, !detail.isEmpty {
                        metadataRow("Blocked detail", value: detail)
                    }
                }
                priorityRow
                repoRow
                if let ordinal = task.ordinal, !task.isChore {
                    metadataRow("Phase", value: "\(ordinal)")
                }
                metadataPRRow(prURL: task.prURL)
                if task.kind == "followup" {
                    let originParts = [
                        task.originTaskShortId.map { "T\($0)" },
                        task.originPrNumber.map { "PR #\($0)" }
                    ].compactMap { $0 }
                    if !originParts.isEmpty {
                        metadataRow("Origin", value: originParts.joined(separator: " / "))
                    }
                }
                sourceChipRow
                if task.sourceAutomationId != nil {
                    automationRow
                }
            }

            WorkDependenciesSection(model: model, taskID: task.id)

            executionsSection

            VStack(alignment: .leading, spacing: 8) {
                Text("Move")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                HStack {
                    ForEach(WorkBoardColumnKey.allCases) { column in
                        Button(column.title) {
                            model.selectWorkCard(task.id)
                            model.moveTask(task.id, to: column)
                        }
                        .disabled(task.boardColumn == column && task.status != "blocked")
                    }
                }
            }

            HStack {
                if task.status == "active" || task.status == "blocked" {
                    Button(task.status == "blocked" ? "Unblock" : "Mark Blocked") {
                        model.selectWorkCard(task.id)
                        model.toggleBlocked(for: task.id)
                    }
                }
                if !task.isChore {
                    Button("Move Up") {
                        model.selectWorkCard(task.id)
                        model.moveSelectedTask(offset: -1)
                    }
                    Button("Move Down") {
                        model.selectWorkCard(task.id)
                        model.moveSelectedTask(offset: 1)
                    }
                }
                Spacer()
                Button("Delete", role: .destructive) {
                    model.selectWorkCard(task.id)
                    model.deleteSelectedWorkItem()
                }
            }
        }
        .padding(20)
        .frame(width: 360, alignment: .leading)
        .onAppear {
            model.loadExecutions(taskId: task.id)
        }
        .sheet(isPresented: $presentingRepoPicker) {
            RepoOverridePicker(
                presentation: model.repoOverridePresentation(for: task),
                recentURLs: model.recentRepoURLs(forProduct: task.productID),
                onCancel: { presentingRepoPicker = false },
                onSelect: { url in
                    model.setRepoOverride(for: task.id, to: url)
                    presentingRepoPicker = false
                },
                onClear: {
                    model.setRepoOverride(for: task.id, to: nil)
                    presentingRepoPicker = false
                }
            )
        }
    }

    /// "Repo:" row inside the popover. Mirrors the CLI `boss <kind>
    /// show` Repo line — resolved URL on the first line, provenance
    /// label below — and trails the row with a `Change…` button that
    /// opens the override picker. Matches the CLI's three-state
    /// vocabulary: override / inherited from product / none-can't-
    /// dispatch.
    @ViewBuilder
    private var repoRow: some View {
        let presentation = model.repoOverridePresentation(for: task)
        VStack(alignment: .leading, spacing: 2) {
            Text("Repo")
                .font(.caption)
                .foregroundStyle(.secondary)
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                VStack(alignment: .leading, spacing: 1) {
                    if let url = presentation.resolvedURL {
                        Text(url)
                            .font(.body)
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .help(url)
                        Text(presentation.provenanceLabel)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    } else {
                        Text(presentation.provenanceLabel)
                            .font(.body)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 8)
                Button("Change…") {
                    presentingRepoPicker = true
                }
                .accessibilityIdentifier("work-card-repo-change")
            }
        }
        .accessibilityIdentifier("work-card-repo-row")
    }

    /// Truncated rendering of the task description so a long body
    /// can't push the trailing metadata (Project, Status, …) off
    /// screen. Caps the visible text to roughly the first six lines
    /// and offers a "Read full description" affordance when the body
    /// has more content or markdown structure worth seeing rendered.
    @ViewBuilder
    private var descriptionSummary: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(descriptionSummaryText)
                .font(.body)
                .lineLimit(6)
                .truncationMode(.tail)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)

            if shouldOfferFullDescription {
                Button {
                    openWindow(
                        id: "markdown-viewer",
                        value: MarkdownViewerContent(
                            title: task.name,
                            markdown: task.description,
                            // Engine-back this description's comments (P529 Phase 2)
                            // on the work-item artifact.
                            artifactKind: WireArtifactKind.workItem,
                            artifactId: task.id
                        )
                    )
                } label: {
                    Label("Read full description", systemImage: "doc.text.magnifyingglass")
                        .font(.callout)
                }
                .buttonStyle(.link)
                .accessibilityIdentifier("work-card-read-full-description")
            }
        }
    }

    /// Plain-text preview used in the popover body. We surface the
    /// first paragraph because longer descriptions are usually a
    /// markdown document (`# heading` lines, fenced code, bullet
    /// lists) — that content reads poorly as raw text and is better
    /// served by the full markdown viewer the affordance opens.
    private var descriptionSummaryText: String {
        let trimmed = task.description.trimmingCharacters(in: .whitespacesAndNewlines)
        let paragraphs = trimmed.components(separatedBy: "\n\n")
        let firstParagraph = paragraphs.first ?? trimmed
        return firstParagraph.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// True when the description has content the truncated preview
    /// can't show (additional paragraphs, more than ~6 lines, or
    /// markdown features like headings or fenced code that only
    /// render meaningfully in the viewer).
    private var shouldOfferFullDescription: Bool {
        let trimmed = task.description.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty { return false }
        if trimmed != descriptionSummaryText { return true }
        if trimmed.components(separatedBy: "\n").count > 6 { return true }
        if trimmed.count > 280 { return true }
        if trimmed.contains("```") { return true }
        if trimmed.contains("\n#") || trimmed.hasPrefix("#") { return true }
        return false
    }

    @ViewBuilder
    private var executionsSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Executions")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("View transcripts…") {
                    openWindow(id: "transcript-viewer", value: TranscriptViewerRef(taskId: task.id))
                }
                .buttonStyle(.link)
                .font(.caption)
                .accessibilityIdentifier("work-card-view-transcripts")
            }
            if let executions = model.executionsByTaskID[task.id] {
                if executions.isEmpty {
                    Text("No executions yet.")
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                } else {
                    let neverStartedAbandoned = executions.filter { $0.status == "abandoned" && $0.startedAt == nil }
                    let visibleExecutions = showAllAbandonedAttempts
                        ? executions
                        : executions.filter { !($0.status == "abandoned" && $0.startedAt == nil) }
                    ScrollView {
                        VStack(alignment: .leading, spacing: 2) {
                            ForEach(visibleExecutions) { exec in
                                Button {
                                    openWindow(
                                        id: "transcript-viewer",
                                        value: TranscriptViewerRef(taskId: task.id, preselectExecutionId: exec.id)
                                    )
                                } label: {
                                    ExecutionRow(exec: exec)
                                        .frame(maxWidth: .infinity, alignment: .leading)
                                }
                                .buttonStyle(.plain)
                            }
                            if !neverStartedAbandoned.isEmpty {
                                Button {
                                    showAllAbandonedAttempts.toggle()
                                } label: {
                                    Text(
                                        showAllAbandonedAttempts
                                            ? "Hide abandoned attempts"
                                            : "\(neverStartedAbandoned.count) abandoned attempts"
                                    )
                                    .font(.caption)
                                }
                                .buttonStyle(.link)
                            }
                        }
                    }
                    .frame(maxHeight: 240)
                }
            } else {
                ProgressView()
                    .controlSize(.small)
            }
        }
    }

    @ViewBuilder
    private func metadataRow(_ label: String, value: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(label)
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(value)
                .font(.body)
        }
    }

    /// Priority row with an inline picker. Editing here fires a
    /// targeted update so authors can re-prioritise a card without
    /// going through the full edit sheet.
    @ViewBuilder
    private var priorityRow: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Priority")
                .font(.caption)
                .foregroundStyle(.secondary)
            Picker(
                "",
                selection: Binding(
                    get: { WorkPriority.parse(task.priority) },
                    set: { newValue in
                        if newValue.rawValue != task.priority {
                            model.setPriority(for: task.id, to: newValue)
                        }
                    }
                )
            ) {
                ForEach(WorkPriority.allCases) { priority in
                    Text(priority.label).tag(priority)
                }
            }
            .labelsHidden()
            .pickerStyle(.menu)
            .fixedSize()
        }
    }

    /// Surface that filed this row, rendered as a small chip below the
    /// PR row. Visible on every card; the chip text is the raw
    /// `created_via` value (`cli`, `mac_app`, `engine_auto`, …) so a
    /// future undocumented source shows up verbatim rather than
    /// silently looking like one of the known values.
    @ViewBuilder
    private var sourceChipRow: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Source")
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(task.createdVia)
                .font(.caption)
                .padding(.horizontal, 8)
                .padding(.vertical, 2)
                .background(
                    Capsule().fill(Color.secondary.opacity(0.15))
                )
        }
    }

    private var automationRow: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Automation")
                .font(.caption)
                .foregroundStyle(.secondary)
            HStack(spacing: 4) {
                Image(systemName: "wand.and.stars")
                    .font(.caption)
                    .foregroundStyle(.purple)
                Text("Created by automation")
                    .font(.caption)
            }
        }
    }

    @ViewBuilder
    private func metadataPRRow(prURL: String?) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("PR")
                .font(.caption)
                .foregroundStyle(.secondary)
            if let prURL, !prURL.isEmpty {
                PRURLLink(urlString: prURL, font: .body)
            } else {
                Text("Not set")
                    .font(.body)
            }
        }
    }
}

/// Picker sheet for the work-item detail Repo: row's `Change…`
/// affordance (per Follow-up chore #12 of
/// `multi-repo-work-modeling.md`). Reuses the create form's
/// recent-repos source — the same per-product distinct-URL set the
/// view model exposes — and adds two row types the create form
/// doesn't need:
///
/// - **Custom URL…** lets the user pin an override the recent set
///   doesn't yet contain (the empirical set bootstraps from the
///   first explicit `--repo`, so brand-new URLs always start
///   custom).
/// - **Clear (inherit from product)** drops the override and falls
///   back to product inheritance. Hidden when there's nothing to
///   clear (current state is already inherited / unresolved).

struct RepoOverridePicker: View {
    let presentation: RepoOverridePresentation
    let recentURLs: [String]
    let onCancel: () -> Void
    let onSelect: (String) -> Void
    let onClear: () -> Void

    @State private var customURL: String = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Change repo")
                .font(.title3.weight(.semibold))

            VStack(alignment: .leading, spacing: 4) {
                Text("Current")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Text(presentation.cliLine)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }

            if !recentURLs.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Recent repos")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(recentURLs, id: \.self) { url in
                            Button(action: { onSelect(url) }) {
                                HStack(spacing: 6) {
                                    Image(systemName: "folder")
                                        .foregroundStyle(.secondary)
                                    Text(shortRepoName(for: url))
                                        .font(.body)
                                    Text(url)
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                        .lineLimit(1)
                                        .truncationMode(.middle)
                                    Spacer(minLength: 4)
                                }
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                            .accessibilityIdentifier("repo-picker-recent-\(url)")
                        }
                    }
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Custom URL")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                HStack(spacing: 8) {
                    TextField(
                        "https://github.com/owner/repo.git",
                        text: $customURL
                    )
                    .textFieldStyle(.roundedBorder)
                    .accessibilityIdentifier("repo-picker-custom-url")
                    Button("Use") {
                        onSelect(customURL)
                    }
                    .disabled(
                        customURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                    )
                    .accessibilityIdentifier("repo-picker-custom-use")
                }
            }

            if canClear {
                Button(action: onClear) {
                    Label("Clear (inherit from product)", systemImage: "arrow.uturn.backward")
                }
                .buttonStyle(.link)
                .accessibilityIdentifier("repo-picker-clear")
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                    .keyboardShortcut(.cancelAction)
            }
        }
        .padding(20)
        .frame(width: 480, alignment: .leading)
        .accessibilityIdentifier("repo-override-picker")
    }

    /// Whether the "Clear (inherit from product)" affordance has any
    /// effect. The override only exists in the `.taskOverride` state;
    /// `.productDefault` and `.none` are already inheriting (or have
    /// nothing to inherit), so clearing would be a no-op and the
    /// button stays hidden to avoid implying an action.
    private var canClear: Bool {
        presentation.provenance == .taskOverride
    }
}

/// Dependencies subsection rendered inside the card detail popover.
/// Mirrors the CLI `boss <kind> show` output: incoming edges
/// (prerequisites) and outgoing edges (dependents) as two short
/// lists, each row hyperlinked to the corresponding card. Collapses
/// to nothing when both lists are empty so the popover doesn't grow
/// taller for cards with no dependencies (design item 12).

struct WorkDependenciesSection: View {
    @ObservedObject var model: ChatViewModel
    let taskID: String

    var body: some View {
        let prereqs = model.dependencyPrereqsByTaskID[taskID] ?? []
        let dependents = model.dependencyDependents(for: taskID)

        if prereqs.isEmpty && dependents.isEmpty {
            EmptyView()
        } else {
            VStack(alignment: .leading, spacing: 10) {
                Text("Dependencies")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .textCase(.uppercase)

                if !prereqs.isEmpty {
                    dependencyList(title: "Prerequisites", rows: prereqs)
                }
                if !dependents.isEmpty {
                    dependencyList(title: "Dependents", rows: dependents)
                }
            }
            .accessibilityIdentifier("work-dependencies-section")
        }
    }

    @ViewBuilder
    private func dependencyList(title: String, rows: [WorkDependencyRow]) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.caption)
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 2) {
                ForEach(rows) { row in
                    WorkDependencyRowView(row: row) {
                        model.selectWorkCard(row.id)
                    }
                }
            }
        }
    }
}

private struct WorkDependencyRowView: View {
    let row: WorkDependencyRow
    let onSelect: () -> Void

    var body: some View {
        Button(action: onSelect) {
            HStack(spacing: 6) {
                Image(systemName: kindSymbol)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(width: 14)
                Text(row.title)
                    .font(.body)
                    .foregroundStyle(linkColor)
                    .underline(isLinkable)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Spacer(minLength: 6)
                Text(statusLabel)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(
                        Capsule(style: .continuous)
                            .fill(Color(nsColor: .quaternaryLabelColor).opacity(0.18))
                    )
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(!isLinkable)
        .help(row.title)
    }

    private var isLinkable: Bool {
        row.kind != .unknown
    }

    private var linkColor: Color {
        isLinkable ? Color.accentColor : .primary
    }

    private var kindSymbol: String {
        switch row.kind {
        case .task:
            return "checkmark.circle"
        case .chore:
            return "wrench.and.screwdriver"
        case .project:
            return "folder"
        case .unknown:
            return "questionmark.circle"
        }
    }

    private var statusLabel: String {
        row.status.replacingOccurrences(of: "_", with: " ").capitalized
    }
}

struct PRURLLink: View {
    let urlString: String
    let font: Font
    /// Board-local disambiguation key from
    /// [[ChatViewModel.ambiguousVisibleRepoNames]]. When set, the label
    /// shortens to `repo#n` for repos not in the set and falls back to
    /// `org/repo#n` for repos that *are*. Pass `nil` to force the full
    /// `org/repo#n` form unconditionally — that's what the detail
    /// popover does, since the popover is the "tooltip-like" surface
    /// the design calls out as always-full.
    var ambiguousRepoNames: Set<String>? = nil

    var body: some View {
        let label = pullRequestLinkLabel(
            for: urlString,
            ambiguousRepoNames: ambiguousRepoNames
        ) ?? urlString
        if let url = URL(string: urlString), url.scheme != nil {
            Link(destination: url) {
                Text(label)
                    .font(font)
                    .foregroundStyle(Color.accentColor)
                    .underline()
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .buttonStyle(.plain)
            .pointerStyle(.link)
            .help(tooltip)
        } else {
            Text(label)
                .font(font)
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
    }

    /// Tooltip surfaces the unambiguous `org/repo#n` form (or, if the
    /// URL isn't a recognisable GitHub PR, the raw URL) so a user who
    /// hovered to verify gets the disambiguating context the shortened
    /// label may have dropped.
    private var tooltip: String {
        if let full = pullRequestLinkLabel(for: urlString, ambiguousRepoNames: nil) {
            return "\(full)\n\(urlString)"
        }
        return urlString
    }
}
