import SwiftUI

extension NavigatorDetailRegistry {
    @MainActor
    static var withAttention: NavigatorDetailRegistry {
        var registry = NavigatorDetailRegistry()
        WorkNavigatorAttention.register(in: &registry)
        return registry
    }
}

/// Attention slot: typed operator questions, attention groups and terminal
/// input waits from the engine projection. Answers reuse the existing
/// kanban / Notifications handlers.
struct WorkNavigatorAttention: View {
    let context: NavigatorDetailContext

    static func register(in registry: inout NavigatorDetailRegistry) {
        registry.register(.attention) { context in
            AnyView(WorkNavigatorAttention(context: context).environmentObject(context.model))
        }
    }

    var body: some View {
        let entries = Self.entries(for: context)
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if entries.isEmpty {
                    Text("No questions or input waits for this task.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                ForEach(Array(entries.enumerated()), id: \.element.id) { index, entry in
                    entryView(entry, index: index, total: entries.count)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.top, 4)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .accessibilityIdentifier("navigator-attention")
    }

    @ViewBuilder
    private func entryView(_ entry: NavigatorAttentionEntry, index: Int, total: Int) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(Self.caption(entry, index: index, total: total, rootID: context.root.id))
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .accessibilityIdentifier("navigator-attention-caption-\(index)")
            switch entry {
            case .question(let task, let presentation):
                OperatorQuestionBody(
                    question: presentation,
                    onAnswer: { context.model.answerOperatorQuestion(for: task, answer: $0) },
                    onDismiss: { context.model.dismissOperatorQuestion(for: task) }
                )
                .disabled(context.isReadOnly)
            case .group(let group):
                AttentionGroupBody(group: group, expectedGeneration: group.generation)
                    .environmentObject(context.model)
                    .disabled(context.isReadOnly)
            case .openInput(let activity):
                NavigatorOpenInputBody(activity: activity, model: context.model)
                    .disabled(context.isReadOnly)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 10)
                .fill(Color(nsColor: .controlBackgroundColor))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10)
                .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
        )
    }

    static func entries(for context: NavigatorDetailContext) -> [NavigatorAttentionEntry] {
        let actions = context.root.navigator?.root?.actions ?? []
        return actions.compactMap { action in
            if action.type == "open_input" {
                guard let activity = action.activity else { return nil }
                return .openInput(activity)
            }
            guard action.type == "attention", let attentionID = action.attentionId else { return nil }
            if let group = context.model.attentionGroup(id: attentionID), group.isOpen {
                return .group(group)
            }
            if let taskID = action.taskId, let task = context.model.task(withID: taskID),
               let presentation = context.model.navigatorOperatorQuestion(for: task),
               presentation.questionID == attentionID || task.operatorQuestion?.id == attentionID {
                return .question(task, presentation)
            }
            if let taskID = action.taskId, let task = context.model.task(withID: taskID),
               let presentation = context.model.navigatorOperatorQuestion(for: task),
               action.kind == "question" {
                return .question(task, presentation)
            }
            return nil
        }
    }

    static func caption(
        _ entry: NavigatorAttentionEntry,
        index: Int,
        total: Int,
        rootID: String
    ) -> String {
        switch entry {
        case .question(let task, _):
            let origin = task.id == rootID
                ? "original task"
                : "\(task.kindLabel.lowercased())\(task.shortID.map { " \($0)" } ?? "")"
            return "Question \(index + 1) of \(total) · from \(origin)"
        case .group(let group):
            return "\(group.kindLabel) \(index + 1) of \(total)"
        case .openInput:
            return "Input needed · \(index + 1) of \(total)"
        }
    }
}

enum NavigatorAttentionEntry: Identifiable {
    case question(WorkTask, OperatorQuestionPresentation)
    case group(AttentionGroup)
    case openInput(NavigatorActivity)

    var id: String {
        switch self {
        case .question(_, let presentation): return "question:\(presentation.questionID)"
        case .group(let group): return "group:\(group.id):\(group.generation)"
        case .openInput(let activity): return "input:\(activity.executionId)"
        }
    }
}

/// A waiting-human execution with no structured question. Reuses the
/// existing agent-focus path rather than a second input surface.
struct NavigatorOpenInputBody: View {
    let activity: NavigatorActivity
    let model: ChatViewModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("The agent is waiting for input in its terminal.")
                .font(.callout)
            if let runID = activity.runId, !runID.isEmpty {
                Button("Open agent input") {
                    model.focusAgentFromWorkCard(runID)
                }
                .accessibilityIdentifier("navigator-open-input")
            } else {
                Text("Agent pane is not available yet.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }
}
