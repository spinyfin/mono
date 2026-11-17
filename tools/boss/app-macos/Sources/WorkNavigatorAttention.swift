import SwiftUI

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
                OwnedAttentionGroupBody(group: group, expectedGeneration: group.generation)
                    .environmentObject(context.model)
                    .disabled(context.isReadOnly)
            case .openInput(let activity):
                NavigatorOpenInputBody(activity: activity, model: context.model)
                    .disabled(context.isReadOnly)
            case .intervention(_, let label):
                NavigatorInterventionBody(label: label)
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
        entries(model: context.model, root: context.root)
    }

    /// Entries the slot can render for `root`, in action order, one per id.
    /// The banner, tab title and slot all derive from this one list.
    static func entries(model: ChatViewModel, root: WorkTask) -> [NavigatorAttentionEntry] {
        let actions = root.navigator?.root?.actions ?? []
        var seen = Set<String>()
        return actions.compactMap { action -> NavigatorAttentionEntry? in
            let entry: NavigatorAttentionEntry?
            if action.type == "open_input" {
                entry = action.activity.map { .openInput($0) }
            } else if action.type == "attention" {
                entry = attentionEntry(for: action, model: model, root: root)
            } else {
                entry = nil
            }
            guard let entry, seen.insert(entry.id).inserted else { return nil }
            return entry
        }
    }

    private static func attentionEntry(
        for action: NavigatorAction,
        model: ChatViewModel,
        root: WorkTask
    ) -> NavigatorAttentionEntry? {
        let attentionID = action.attentionId ?? ""
        if action.kind == "intervention" {
            let id = attentionID.isEmpty ? (action.taskId ?? root.id) : attentionID
            return .intervention(id: id, label: interventionLabel(attentionID: attentionID))
        }
        if !attentionID.isEmpty, let group = model.attentionGroup(id: attentionID), group.isOpen {
            return .group(group)
        }
        guard let taskID = action.taskId, let task = model.task(withID: taskID),
              let presentation = model.navigatorOperatorQuestion(for: task)
        else { return nil }
        // An explicit id must match this task's question; only an action with
        // no id falls back to "the task's question".
        if attentionID.isEmpty ? action.kind == "question" : presentation.questionID == attentionID {
            return .question(task, presentation)
        }
        return nil
    }

    /// Engine attention ids are `intervention:<task>:<reason>`. The family
    /// `reasonLabel` is the highest-ranked human action (Question before
    /// Intervention), so this card names the action's own reason.
    static func interventionLabel(attentionID: String) -> String {
        let parts = attentionID.split(separator: ":", omittingEmptySubsequences: false)
        guard parts.count >= 3, parts[0] == "intervention" else {
            return "Intervention"
        }
        let reason = String(parts.dropFirst(2).joined(separator: ":"))
        switch reason {
        case "ci_failure_exhausted":
            return WorkBlockedBadge.label(forReason: reason)
        case "deliberate_park":
            return "Waiting on you"
        case "recovery_attention":
            return "Recovery"
        case "workspace_positioning_refused":
            return "Workspace blocked"
        default:
            return "Intervention"
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
        case .intervention(_, let label):
            return "\(label) · \(index + 1) of \(total)"
        }
    }
}

enum NavigatorAttentionEntry: Identifiable {
    case question(WorkTask, OperatorQuestionPresentation)
    case group(AttentionGroup)
    case openInput(NavigatorActivity)
    case intervention(id: String, label: String)

    var id: String {
        switch self {
        case .question(_, let presentation): return "question:\(presentation.questionID)"
        case .group(let group): return "group:\(group.id):\(group.generation)"
        case .openInput(let activity): return "input:\(activity.executionId)"
        case .intervention(let id, _): return "intervention:\(id)"
        }
    }
}

/// Engine-mapped intervention with no structured question. Read-only: the
/// person acts from Activity, Kanban, or the reason the engine already named.
struct NavigatorInterventionBody: View {
    let label: String

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(label)
                .font(.callout)
            Text("This task needs a person to act. There is no question to answer here.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .accessibilityIdentifier("navigator-intervention")
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
