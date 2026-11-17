import SwiftUI

/// Revision, execution and dependency history for the selected root.
struct WorkNavigatorHistory: View {
    let context: NavigatorDetailContext
    @Environment(\.openWindow) private var openWindow
    @State private var executionsToken = UUID()

    private var model: ChatViewModel { context.model }
    private var root: WorkTask { context.root }
    private var family: [WorkTask] { model.navigatorFamily(of: root) }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if let origin = originLabel {
                    LabeledContent("Origin", value: origin)
                }
                revisionList
                WorkDependenciesSection(model: model, taskID: root.id) { row in
                    model.navigateNavigatorDependency(row)
                }
                executionsList
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.top, 4)
        }
        .onAppear {
            for member in family {
                model.beginExecutionsConsumer(taskID: member.id, token: executionsToken)
            }
        }
        .onDisappear {
            for member in family {
                model.endExecutionsConsumer(taskID: member.id, token: executionsToken)
            }
        }
    }

    private var originLabel: String? {
        guard root.kind == "followup" else { return nil }
        let parts = [
            root.originTaskShortId.map { "T\($0)" },
            root.originPrNumber.map { "PR #\($0)" },
        ].compactMap { $0 }
        return parts.isEmpty ? nil : parts.joined(separator: " / ")
    }

    @ViewBuilder
    private var revisionList: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Revisions").font(.headline)
            ForEach(family, id: \.id) { member in
                Button {
                    model.selectNavigatorRoot(root.id, askID: member.id != root.id ? member.id : nil)
                    if member.id != root.id {
                        model.setNavigatorDocument(.currentAsk, rootID: root.id)
                    } else {
                        model.setNavigatorDocument(.original, rootID: root.id)
                    }
                    model.navigator.tab = .activity
                } label: {
                    HStack {
                        Text(member.id == root.id ? "Original" : member.name)
                            .lineLimit(1)
                        Spacer()
                        Text(member.status.replacingOccurrences(of: "_", with: " ").capitalized)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                .buttonStyle(.plain)
                if !member.description.isEmpty, member.id != root.id {
                    Text(member.description)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(3)
                }
            }
        }
        .accessibilityIdentifier("navigator-history-revisions")
    }

    @ViewBuilder
    private var executionsList: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text("Executions").font(.headline)
                Spacer()
                Button("View transcripts…") {
                    openWindow(id: "transcript-viewer", value: TranscriptViewerRef(taskId: root.id))
                }
                .buttonStyle(.link)
                .font(.caption)
            }
            let executions = family.flatMap { model.executionsByTaskID[$0.id] ?? [] }
            if family.contains(where: { model.executionsByTaskID[$0.id] == nil }) {
                ProgressView().controlSize(.small)
            } else if executions.isEmpty {
                Text("No executions yet.").font(.caption).foregroundStyle(.tertiary)
            } else {
                ForEach(executions) { exec in
                    Button {
                        openWindow(
                            id: "transcript-viewer",
                            value: TranscriptViewerRef(taskId: exec.workItemId, preselectExecutionId: exec.id)
                        )
                    } label: {
                        ExecutionRow(exec: exec)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .buttonStyle(.plain)
                }
            }
        }
        .accessibilityIdentifier("navigator-history-executions")
    }
}
