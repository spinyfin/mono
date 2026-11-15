import SwiftUI

/// Bodies register against stable slots. Their identity follows root + slot,
/// never section, reason or execution; richer integrations can own local drafts.
struct NavigatorDetailContext {
    let model: ChatViewModel
    let root: WorkTask
    let ask: WorkTask?
    let isReadOnly: Bool
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
            VStack(alignment: .leading, spacing: 12) {
                Text(root.name).font(.title2).textSelection(.enabled)
                Text([model.projectName(for: root.projectID), root.navigator?.root?.reasonLabel]
                    .compactMap { $0 }.joined(separator: " · ")).foregroundStyle(.secondary)
                if let notice = model.navigatorSelectionNotice {
                    HStack {
                        Text(notice).font(.callout)
                        if model.task(withID: root.id) != nil, root.deletedAt == nil, root.status != "archived" {
                            Button("Reveal") { model.revealNavigatorSelection() }
                        }
                    }
                }
                if root.navigator?.root?.actions.isEmpty == false {
                    Button("Attention needed (\(root.navigator?.root?.actions.count ?? 0))") {
                        model.navigator.tab = .attention
                    }
                }
                Picker("Detail", selection: $model.navigator.tab) {
                    ForEach(NavigatorDetailSlot.allCases.filter { model.navigator.tabs.contains($0) }) { slot in
                        Text(slot.title).tag(slot)
                    }
                }
                .pickerStyle(.segmented)
                // Keep every registered body mounted while switching tabs.
                ZStack(alignment: .topLeading) {
                    ForEach(NavigatorDetailSlot.allCases.filter { model.navigator.tabs.contains($0) }) { slot in
                        slotBody(slot, root: root)
                            .opacity(model.navigator.tab == slot ? 1 : 0)
                            .allowsHitTesting(model.navigator.tab == slot)
                            .disabled(model.navigator.tab != slot)
                            .accessibilityHidden(model.navigator.tab != slot)
                    }
                }
                .id(root.id)
            }
            .padding(16)
        } else {
            ContentUnavailableView("Select a task", systemImage: "sidebar.left",
                                   description: Text("See its brief, activity and review here."))
        }
    }

    @ViewBuilder
    private func slotBody(_ slot: NavigatorDetailSlot, root: WorkTask) -> some View {
        let readOnly = !model.isConnected || root.deletedAt != nil || root.status == "archived"
            || model.task(withID: root.id) == nil
        let ask = model.navigator.askID.flatMap { model.task(withID: $0) }
        let context = NavigatorDetailContext(model: model, root: root, ask: ask, isReadOnly: readOnly)
        if let body = registry.bodies[slot] {
            body(context).disabled(readOnly)
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
            if let pr = context.root.prURL, let url = URL(string: pr) {
                Link("Open pull request", destination: url)
            }
            if let health = context.root.ciRequiredState { Text("CI: \(health)").font(.caption) }
            if let evidence = context.root.navigator?.root?.mergeEvidence {
                Text(evidence.mergedAt.map { "Merged: \($0)" } ?? "Merge observed: \(evidence.observedAt)")
                    .font(.caption)
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
