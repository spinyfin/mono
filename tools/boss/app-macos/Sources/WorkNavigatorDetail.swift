import SwiftUI

/// Bodies register against stable slots. Their identity follows root + slot,
/// never section, reason or execution; richer integrations can own local drafts.
struct NavigatorDetailContext {
    let model: ChatViewModel
    let root: WorkTask
    let ask: WorkTask?
    let isReadOnly: Bool

    /// Inline guide comments are read-only only for durable conditions. A
    /// connection blip must not close an in-progress composer; offline comments
    /// ride the engine's outbound retry ledger, as in the standalone reader.
    @MainActor var isCommentsReadOnly: Bool {
        root.deletedAt != nil || root.status == "archived" || model.task(withID: root.id) == nil
    }
}

struct NavigatorDetailRegistry {
    typealias Body = @MainActor (NavigatorDetailContext) -> AnyView
    var bodies: [NavigatorDetailSlot: Body] = [:]

    mutating func register(_ slot: NavigatorDetailSlot, body: @escaping Body) {
        bodies[slot] = body
    }
}

struct WorkNavigatorDetail: View {
    @ObservedObject var model: ChatViewModel
    var registry = NavigatorDetailRegistry()

    var body: some View {
        if let root = model.navigatorSelectedTask {
            VStack(alignment: .leading, spacing: 8) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(root.name).font(.title3.weight(.semibold)).lineLimit(2)
                        .textSelection(.enabled)
                    if let status = root.navigator?.root?.reasonLabel {
                        Text(status)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .padding(.horizontal, 8)
                            .padding(.vertical, 2)
                            .background(Capsule().fill(Color.secondary.opacity(0.15)))
                            .fixedSize()
                            .accessibilityIdentifier("navigator-status-pill")
                    }
                }
                if let project = model.projectName(for: root.projectID), !project.isEmpty {
                    Text(project).font(.caption).foregroundStyle(.secondary)
                }
                if let pr = root.prURL, let url = URL(string: pr) {
                    Link("Open pull request", destination: url)
                }
                if let evidence = root.navigator?.root?.mergeEvidence {
                    Text(evidence.mergedAt.map { "Merged: \($0)" } ?? "Merge observed: \(evidence.observedAt)")
                        .font(.caption)
                }
                if let projection = root.navigator?.root {
                    let facts = NavigatorRowPresentation(row: NavigatorRow(task: root, projection: projection, matchingAskID: nil), project: nil, width: 520).healthFacts
                    Text(facts.joined(separator: " · ")).font(.caption).foregroundStyle(.secondary)
                }
                if let notice = model.navigatorSelectionNotice {
                    HStack {
                        Text(notice).font(.callout)
                        if model.task(withID: root.id) != nil, root.deletedAt == nil, root.status != "archived" {
                            Button("Reveal") { model.revealNavigatorSelection() }
                        }
                    }
                }
                // Count what the Attention slot can actually render, not raw engine actions.
                let actionCount = WorkNavigatorAttention.entries(model: model, root: root).count
                let visibleTabs = Self.visibleTabs(model: model)
                if actionCount > 0, model.navigator.tab != .attention {
                    Button("Attention needed (\(actionCount))") {
                        model.navigator.tab = .attention
                    }
                    .accessibilityIdentifier("navigator-attention-banner")
                }
                Picker("Detail", selection: $model.navigator.tab) {
                    ForEach(visibleTabs) { slot in
                        Text(slot.title(actionCount: actionCount)).tag(slot)
                    }
                }
                .labelsHidden()
                .pickerStyle(.segmented)
                // Keep every registered body mounted while switching tabs.
                ZStack(alignment: .topLeading) {
                    ForEach(visibleTabs) { slot in
                        slotBody(slot, root: root)
                            .opacity(model.navigator.tab == slot ? 1 : 0)
                            .allowsHitTesting(model.navigator.tab == slot)
                            .disabled(model.navigator.tab != slot)
                            .accessibilityHidden(model.navigator.tab != slot)
                    }
                }
                .id(root.id)
            }
            .padding(12)
        } else {
            ContentUnavailableView("Select a task", systemImage: "sidebar.left",
                                   description: Text("See its brief, activity and review here."))
        }
    }

    /// Slots follow the registered tab set for the selected root. Attention
    /// stays listed after its last entry resolves so the empty state remains
    /// reachable and every registered body stays mounted.
    static func visibleTabs(
        model: ChatViewModel
    ) -> [NavigatorDetailSlot] {
        // Presence is the registered tab set on the selection, not the
        // current entry list, so an empty Attention slot stays mounted.
        NavigatorDetailSlot.allCases.filter { model.navigator.tabs.contains($0) }
    }

    @ViewBuilder
    private func slotBody(_ slot: NavigatorDetailSlot, root: WorkTask) -> some View {
        let readOnly = !model.isConnected || root.deletedAt != nil || root.status == "archived"
            || model.task(withID: root.id) == nil
        let ask = model.navigator.askID.flatMap { model.task(withID: $0) }
        let context = NavigatorDetailContext(model: model, root: root, ask: ask, isReadOnly: readOnly)
        if let body = registry.bodies[slot] {
            body(context).disabled(readOnly && slot != .review && slot != .history)
        } else {
            NavigatorFallbackDetail(slot: slot, context: context)
        }
    }
}

private struct NavigatorFallbackDetail: View {
    let slot: NavigatorDetailSlot
    let context: NavigatorDetailContext
    @Environment(\.openWindow) private var openWindow

    private var unavailable: String {
        switch slot {
        case .activity: "Inline brief editing and agent terminal are not available in Navigator yet."
        case .attention: "Inline question answers are not available in Navigator yet. Use Kanban for worker questions or open Notifications for project questions."
        case .review: "Inline review guide is not available in Navigator yet. Diff viewer is a placeholder."
        case .history: "Revision and execution history is not available in Navigator yet."
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label(unavailable, systemImage: "info.circle").font(.callout).foregroundStyle(.secondary)
            if slot == .attention {
                HStack {
                    Button("Use Kanban") { context.model.setWorkViewMode(.kanban) }
                    Button("Open Notifications") { openWindow(id: "attentions") }
                }
            }
            if let ask = context.ask, ask.id != context.root.id {
                Text("Current ask: \(ask.name)").font(.headline)
                Text(ask.description).textSelection(.enabled).lineLimit(6)
            }
            MarkdownDocumentChrome(title: "Original brief", source: context.root.description, commentsEnabled: false)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }
}
