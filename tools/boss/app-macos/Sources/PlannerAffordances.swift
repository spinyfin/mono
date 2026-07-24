import AppKit
import os.log
import SwiftUI
import UpdateCore

struct ProjectDesignDocAffordance: View {
    @ObservedObject var model: ChatViewModel
    let project: WorkProject

    var body: some View {
        if let presentation = ProjectDesignDocAffordancePresentation.from(
            state: model.designDocStateByProjectID[project.id] ?? .notSet
        ) {
            Button {
                model.openProjectDesignDoc(project)
            } label: {
                Image(systemName: presentation.systemImage)
                    .font(.caption)
                    .foregroundStyle(presentation.tint)
                    .accessibilityLabel(presentation.accessibilityLabel)
            }
            .buttonStyle(.plain)
            .help(presentation.tooltip)
        }
    }
}

/// Pure-data presentation chosen for a `ProjectDesignDocState`. Lives
/// outside the view so tests can assert "this state renders this
/// icon" without spinning up a SwiftUI host — the kanban view is a
/// thin reflection of these fields.

struct ProjectDesignDocAffordancePresentation: Equatable {
    let systemImage: String
    let tooltip: String
    let accessibilityLabel: String
    let kind: Kind

    enum Kind: Equatable {
        case resolved
        case broken
    }

    var tint: Color {
        switch kind {
        case .resolved:
            return .secondary
        case .broken:
            return .orange
        }
    }

    /// Map a `ProjectDesignDocState` to its kanban presentation. Returns
    /// `nil` for `.notSet` so the kanban hides the affordance entirely
    /// — the design doc spec (Q3) wants no icon when the pointer is
    /// unset, distinct from the warning glyph used for broken pointers.
    static func from(state: ProjectDesignDocState) -> ProjectDesignDocAffordancePresentation? {
        switch state {
        case .notSet:
            return nil
        case .resolved(let resolved, _, _, _):
            let repoBase = repoBasename(from: resolved.repoRemoteURL)
            let tooltip = "\(repoBase):\(resolved.path)"
            return ProjectDesignDocAffordancePresentation(
                systemImage: "doc.text",
                tooltip: tooltip,
                accessibilityLabel: "Open design doc",
                kind: .resolved
            )
        case .broken(let reason):
            return ProjectDesignDocAffordancePresentation(
                systemImage: "exclamationmark.triangle",
                tooltip: "Design doc pointer is broken: \(reason)",
                accessibilityLabel: "Design doc pointer is broken",
                kind: .broken
            )
        }
    }

    /// Pull the `owner/repo` slug out of a GitHub URL for the hover
    /// tooltip. Falls back to the raw URL when the path isn't
    /// recognisable so we never render an empty `:path`. Handles
    /// both `https://github.com/foo/bar.git` and SCP-style
    /// `git@github.com:foo/bar.git` — `URL(string:)` accepts the
    /// SCP form on macOS but treats `git@github.com` as the scheme
    /// and leaves `path` empty, so the scheme check below routes
    /// scheme-less inputs through the colon-split branch.
    static func repoSlug(from repoURL: String) -> String {
        repoBasename(from: repoURL)
    }

    private static func repoBasename(from repoURL: String) -> String {
        if let url = URL(string: repoURL), url.host != nil {
            let parts = url.path
                .split(separator: "/", omittingEmptySubsequences: true)
                .map(String.init)
            if parts.count >= 2 {
                let owner = parts[0]
                let repo = parts[1].hasSuffix(".git")
                    ? String(parts[1].dropLast(4))
                    : parts[1]
                return "\(owner)/\(repo)"
            }
        }
        if let scpRange = repoURL.range(of: ":") {
            let path = String(repoURL[scpRange.upperBound...])
            let trimmed = path.hasSuffix(".git") ? String(path.dropLast(4)) : path
            return trimmed
        }
        return repoURL
    }
}

// ===========================================================================
// Planner review/release/undo surface (design: tools/boss/docs/designs/
// auto-populate-project-tasks-on-design-pr-merge.md, task 10). Thin client
// over `list_planner_runs` / `release_project` / `unpopulate_project`: a
// kanban project-header accessory summarising the project's latest planner
// run, a popover with the Release/Undo actions, and a full inspector sheet
// (raw model output + rationale) for the whole audit trail.
// ===========================================================================

/// Per-project Planner affordance for the kanban project-section header,
/// mirroring [[ProjectDesignDocAffordance]]. Fetches the project's planner
/// runs on first appearance and stays empty until the first reply lands
/// (or the project has never been planned), then shows an outcome-tinted
/// icon that opens [[PlannerRunPopoverView]].

struct PlannerRunAffordance: View {
    @ObservedObject var model: ChatViewModel
    let project: WorkProject
    @State private var isPopoverPresented = false
    @State private var hasRequestedRuns = false

    private var latestRun: PlannerRun? { model.latestPlannerRun(forProjectID: project.id) }

    var body: some View {
        Group {
            if let latestRun {
                Button {
                    isPopoverPresented = true
                } label: {
                    Image(systemName: systemImage(for: latestRun))
                        .font(.caption)
                        .foregroundStyle(tint(for: latestRun))
                        .accessibilityLabel("Planner: \(latestRun.outcomeLabel)")
                }
                .buttonStyle(.plain)
                .help(latestRun.outcomeLabel)
                .popover(isPresented: $isPopoverPresented) {
                    PlannerRunPopoverView(model: model, project: project, run: latestRun)
                }
            }
        }
        .onAppear {
            guard !hasRequestedRuns else { return }
            hasRequestedRuns = true
            model.refreshPlannerRuns(projectID: project.id)
        }
    }

    private func systemImage(for run: PlannerRun) -> String {
        switch run.outcome {
        case "staged": return "tray.and.arrow.down.fill"
        case "applied": return "checkmark.circle"
        case "running": return "hourglass"
        default: return "exclamationmark.circle"
        }
    }

    private func tint(for run: PlannerRun) -> Color {
        switch run.outcome {
        case "staged": return .accentColor
        case "applied", "running": return .secondary
        default: return .orange
        }
    }
}

/// Compact popover shown from [[PlannerRunAffordance]]: the latest run's
/// outcome and summary, a Release action while staged, an Undo action
/// while the batch could still be reverted, and a link into the full
/// [[PlannerRunInspectorView]].

private struct PlannerRunPopoverView: View {
    @ObservedObject var model: ChatViewModel
    let project: WorkProject
    let run: PlannerRun

    private var isBusy: Bool { model.plannerActionInFlightProjectIDs.contains(project.id) }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 6) {
                Image(systemName: "sparkle.magnifyingglass")
                    .foregroundStyle(.secondary)
                Text("Planner").font(.headline)
                Spacer(minLength: 0)
            }
            Text(run.outcomeLabel)
                .font(.subheadline.weight(.medium))
            if let summary = run.resultSummary, !summary.isEmpty {
                PlannerResultSummaryView(headline: run.plannerFailureHeadline, raw: summary)
            }
            Divider()
            HStack(spacing: 8) {
                if run.isStaged {
                    Button {
                        model.releaseProject(projectID: project.id)
                    } label: {
                        Label("Release", systemImage: "play.fill")
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(isBusy)
                }
                if run.isStaged || run.isApplied {
                    PlannerUndoButton(model: model, project: project, run: run, isBusy: isBusy)
                }
                Spacer(minLength: 0)
                if isBusy {
                    ProgressView().controlSize(.small)
                }
            }
            Button {
                model.openPlannerInspector(projectID: project.id)
            } label: {
                Text("View all planner runs…")
            }
            .buttonStyle(.link)
            .font(.caption)
        }
        .padding(14)
        .frame(minWidth: 300, maxWidth: 460, alignment: .leading)
    }
}

/// Shared "Undo" control with a confirmation dialog — deleting a staged
/// batch is reversible only via re-plan, so it warrants a confirm step
/// unlike Release (which is purely additive: flip `autostart`).

private struct PlannerUndoButton: View {
    @ObservedObject var model: ChatViewModel
    let project: WorkProject
    let run: PlannerRun
    let isBusy: Bool
    @State private var isConfirmingUndo = false

    var body: some View {
        Button(role: .destructive) {
            isConfirmingUndo = true
        } label: {
            Label("Undo", systemImage: "arrow.uturn.backward")
        }
        .buttonStyle(.bordered)
        .disabled(isBusy)
        .confirmationDialog(
            "Undo this planner run?",
            isPresented: $isConfirmingUndo,
            titleVisibility: .visible
        ) {
            Button("Delete staged tasks", role: .destructive) {
                model.unpopulateProject(projectID: project.id, runID: run.id)
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(
                "Deletes the tasks this run staged that haven't been released and dispatched yet. "
                    + "Tasks already in progress are preserved."
            )
        }
    }
}

/// Full planner-run audit view for one project ("planner-run inspector
/// (raw output + rationale)" — task 10). Lists every `planner_runs` row,
/// newest first, each expandable to its rationale notes, effort-
/// classification audit lines, and the verbatim raw model output.

struct PlannerRunInspectorView: View {
    @ObservedObject var model: ChatViewModel
    let project: WorkProject
    @Environment(\.dismiss) private var dismiss

    private var runs: [PlannerRun] { model.plannerRuns(forProjectID: project.id) }
    private var isBusy: Bool { model.plannerActionInFlightProjectIDs.contains(project.id) }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            if runs.isEmpty {
                emptyState
            } else {
                List {
                    ForEach(runs) { run in
                        PlannerRunRow(model: model, project: project, run: run, isBusy: isBusy)
                    }
                }
                .listStyle(.inset)
            }
        }
        .frame(minWidth: 560, minHeight: 420)
        .onAppear { model.refreshPlannerRuns(projectID: project.id) }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: "sparkle.magnifyingglass")
                .foregroundStyle(.secondary)
            Text("Planner runs")
                .font(.headline)
            Text(project.name)
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .lineLimit(1)
            Spacer(minLength: 0)
            Button("Done") { dismiss() }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }

    private var emptyState: some View {
        VStack(spacing: 8) {
            Image(systemName: "tray")
                .font(.system(size: 28))
                .foregroundStyle(.tertiary)
            Text("No planner runs recorded")
                .font(.headline)
            Text("Runs appear here once the design doc merges, or after an operator runs `boss project plan`.")
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(32)
    }
}

/// One expandable row in [[PlannerRunInspectorView]] — collapsed shows the
/// outcome, caller, and timestamp; expanded adds rationale, effort audit,
/// raw output, and the Release/Undo actions when this run is still live.

private struct PlannerRunRow: View {
    @ObservedObject var model: ChatViewModel
    let project: WorkProject
    let run: PlannerRun
    let isBusy: Bool
    @State private var isExpanded: Bool

    init(model: ChatViewModel, project: WorkProject, run: PlannerRun, isBusy: Bool) {
        self.model = model
        self.project = project
        self.run = run
        self.isBusy = isBusy
        _isExpanded = State(initialValue: run.isStaged)
    }

    var body: some View {
        DisclosureGroup(isExpanded: $isExpanded) {
            VStack(alignment: .leading, spacing: 10) {
                if let notes = run.notes, !notes.isEmpty {
                    section(title: "Rationale") {
                        PlannerRunRationaleText(text: notes)
                    }
                }
                if let summary = run.resultSummary, !summary.isEmpty {
                    section(title: "Result") {
                        PlannerResultSummaryView(headline: run.plannerFailureHeadline, raw: summary)
                    }
                }
                if hasDebugInfo {
                    debugInfoDisclosure
                }
                if run.isStaged || run.isApplied {
                    actions
                }
            }
            .padding(.top, 6)
            .padding(.leading, 16)
        } label: {
            HStack(alignment: .top, spacing: 8) {
                Circle().fill(tint).frame(width: 8, height: 8).padding(.top, 5)
                VStack(alignment: .leading, spacing: 2) {
                    Text(run.outcomeLabel).font(.subheadline.weight(.medium))
                    Text("\(run.caller) · \(run.createdAt)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    if let summary = run.resultSummary, !summary.isEmpty {
                        Text(summary.unescapedForDisplay)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                            .truncationMode(.tail)
                    }
                }
                Spacer(minLength: 0)
            }
        }
    }

    private var tint: Color {
        switch run.outcome {
        case "staged": return .accentColor
        case "applied": return .green
        case "running": return .secondary
        default: return .orange
        }
    }

    @ViewBuilder
    private func section<Content: View>(title: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            content()
        }
    }

    /// `true` when this run has machine-audit metadata (effort-classification
    /// lines, verbatim model output) worth keeping around for a rare
    /// diagnostic look — but not worth showing an operator by default.
    private var hasDebugInfo: Bool {
        !run.effortAuditLines.isEmpty || !(run.rawOutput ?? "").isEmpty
    }

    /// Machine-audit metadata, collapsed behind its own disclosure so it
    /// never shows by default alongside the operator-facing rationale.
    /// Always rendered in a monospace block, never inline with prose.
    @ViewBuilder
    private var debugInfoDisclosure: some View {
        DisclosureGroup("Debug info") {
            VStack(alignment: .leading, spacing: 10) {
                let auditLines = run.effortAuditLines
                if !auditLines.isEmpty {
                    section(title: "Effort classification") {
                        VStack(alignment: .leading, spacing: 2) {
                            ForEach(auditLines, id: \.self) { line in
                                Text(line).font(.caption.monospaced())
                            }
                        }
                    }
                }
                if let rawOutput = run.rawOutput, !rawOutput.isEmpty {
                    section(title: "Raw model output") {
                        ScrollView {
                            Text(rawOutput)
                                .font(.caption.monospaced())
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                        .frame(maxHeight: 200)
                    }
                }
            }
            .padding(.top, 6)
        }
        .font(.caption.weight(.semibold))
        .foregroundStyle(.secondary)
    }

    @ViewBuilder
    private var actions: some View {
        HStack(spacing: 8) {
            if run.isStaged {
                Button {
                    model.releaseProject(projectID: project.id)
                } label: {
                    Label("Release", systemImage: "play.fill")
                }
                .buttonStyle(.borderedProminent)
                .disabled(isBusy)
            }
            PlannerUndoButton(model: model, project: project, run: run, isBusy: isBusy)
            if isBusy {
                ProgressView().controlSize(.small)
            }
        }
    }
}

/// Wrapped, capped rendering of a Planner run's `result_summary` — populated
/// verbatim by the engine, so a `planner_failed` run can carry an arbitrarily
/// long, multiply-escaped serde diagnostic (see `populator.rs`). Leads with a
/// short human-readable `headline` when one is available (see
/// `PlannerRun.plannerFailureHeadline`), always wraps rather than laying the
/// text out on one unbounded line, caps the raw text to a few lines behind a
/// "Show more" disclosure so a long payload can't blow out the surrounding
/// layout, and offers a copy button for the verbatim text a developer would
/// need to debug something like a schema mismatch.

private struct PlannerResultSummaryView: View {
    let headline: String?
    let raw: String
    @State private var isExpanded = false

    /// `true` only for a `planner_failed` run's diagnostic — the only case
    /// with a non-nil `headline` (see `PlannerRun.plannerFailureHeadline`).
    /// Successful-run summaries (e.g. "created 5 tasks, 3 edges") are short
    /// prose, not diagnostics, so they render plain with no monospace or
    /// copy chrome.
    private var isFailure: Bool { headline != nil }

    private var displayText: String { raw.unescapedForDisplay }
    private var needsDisclosure: Bool { PlannerResultSummaryLayout.needsDisclosure(for: displayText) }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            if let headline {
                Text(headline)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if isExpanded {
                ScrollView {
                    Text(displayText)
                        .font(isFailure ? .caption.monospaced() : .caption)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxHeight: 220)
            } else {
                Text(displayText)
                    .font(isFailure ? .caption.monospaced() : .caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(PlannerResultSummaryLayout.collapsedLineLimit)
                    .truncationMode(.tail)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            HStack(spacing: 10) {
                if needsDisclosure {
                    Button(isExpanded ? "Show less" : "Show more") {
                        withAnimation(.easeInOut(duration: 0.15)) { isExpanded.toggle() }
                    }
                    .buttonStyle(.link)
                    .font(.caption2)
                }
                if isFailure {
                    Button {
                        let pb = NSPasteboard.general
                        pb.clearContents()
                        pb.setString(raw, forType: .string)
                    } label: {
                        Label("Copy", systemImage: "doc.on.doc")
                    }
                    .buttonStyle(.borderless)
                    .font(.caption2)
                    .help("Copy the raw, unmodified text to the clipboard")
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// Pure, testable layout rules backing [[PlannerResultSummaryView]]'s "Show
/// more" disclosure — kept as a free-standing internal type (rather than
/// nested in the file-private view) so `PlannerFailureRenderingTests` can
/// exercise the predicate via `@testable import` without needing the view
/// itself to be visible outside this file.

enum PlannerResultSummaryLayout {
    /// Below this, and within `collapsedLineLimit` lines, the text always
    /// fits comfortably in the collapsed state, so "Show more" would be dead
    /// chrome.
    static let disclosureCharacterThreshold = 200
    /// Line cap applied to the collapsed `Text` — shared with
    /// `needsDisclosure` so the two can't drift apart.
    static let collapsedLineLimit = 4

    /// `true` when `text` would be truncated by either dimension of the
    /// collapsed rendering: too many characters, or (independently) more
    /// newline-separated lines than the collapsed line cap allows. A short
    /// multi-line payload (e.g. eight 20-character lines) is well under the
    /// character threshold but still gets clipped by `lineLimit`, so both
    /// checks are required.
    static func needsDisclosure(for text: String) -> Bool {
        if text.count > disclosureCharacterThreshold { return true }
        let lineCount = text.split(separator: "\n", omittingEmptySubsequences: false).count
        return lineCount > collapsedLineLimit
    }
}

/// Renders a Planner run's free-text rationale as legible prose: blank-line
/// separated paragraphs get real spacing and line height instead of being
/// dumped into a single dense `Text`.

private struct PlannerRunRationaleText: View {
    let text: String

    private var paragraphs: [String] {
        text
            .components(separatedBy: "\n\n")
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            ForEach(Array(paragraphs.enumerated()), id: \.offset) { _, paragraph in
                Text(paragraph)
                    .font(.callout)
                    .lineSpacing(4)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

struct AgentActivityDot: View {
    let state: AgentActivityState

    var body: some View {
        Group {
            if case .dispatchPending = state {
                Image(systemName: "hourglass")
                    .font(.system(size: 9, weight: .medium))
                    .foregroundStyle(Color(nsColor: .tertiaryLabelColor))
                    .frame(width: 7, height: 7)
            } else {
                Circle()
                    .fill(fillColor)
                    .frame(width: 7, height: 7)
            }
        }
        .help(state.tooltip)
        .accessibilityLabel(state.tooltip)
    }

    private var fillColor: Color {
        switch state {
        case .active:
            return .green
        case .waiting:
            return .yellow
        case .errored:
            return .red
        case .none:
            return Color(nsColor: .tertiaryLabelColor)
        case .dispatchPending:
            return Color(nsColor: .tertiaryLabelColor)
        }
    }
}
