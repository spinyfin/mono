import SwiftUI

extension WorkTask {
    var followupOriginDescription: String? {
        guard kind == "followup" else { return nil }
        let parts = [
            originTaskShortId.map { "T\($0)" },
            originPrNumber.map { "PR #\($0)" },
        ].compactMap { $0 }
        return parts.isEmpty ? nil : parts.joined(separator: " / ")
    }
}

/// Shared execution history, including abandoned attempts and load failures.
struct WorkExecutionsList: View {
    @ObservedObject var model: ChatViewModel
    let taskIDs: [String]
    let transcriptTaskID: String
    /// False while the list is mounted but hidden. Consumers still register
    /// so History can prefetch; invalidations then mark the cache stale and
    /// the next activation refetches. Visible lists keep live invalidations.
    var isActive = true
    @Environment(\.openWindow) private var openWindow
    @State private var showAllAbandonedAttempts = false
    @State private var consumer = WorkExecutionsConsumer()

    func updateConsumer(_ consumer: WorkExecutionsConsumer) {
        consumer.update(taskIDs: Set(taskIDs), model: model, live: isActive)
    }

    /// Loaded-row counts in `taskIDs` order. Cheaper than mapping every
    /// execution id on each body evaluation, and still changes when a fetch
    /// lands or the family changes.
    var executionsLoadedCounts: [Int] {
        taskIDs.map { model.executionsByTaskID[$0]?.count ?? -1 }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Executions").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("View transcripts…") {
                    openWindow(id: "transcript-viewer", value: TranscriptViewerRef(taskId: transcriptTaskID))
                }
                .buttonStyle(.link)
                .font(.caption)
                .accessibilityIdentifier("work-card-view-transcripts")
            }
            ForEach(taskIDs, id: \.self) { id in
                if let error = model.executionsLoadFailureByTaskID[id] {
                    Text(error).font(.caption).foregroundStyle(.red)
                    Button("Retry loading executions") { model.loadExecutions(taskId: id) }
                        .buttonStyle(.link)
                }
            }
            let executions = taskIDs.flatMap { model.executionsByTaskID[$0] ?? [] }
            let abandoned = executions.filter { $0.status == "abandoned" && $0.startedAt == nil }
            let visible = showAllAbandonedAttempts ? executions
                : executions.filter { !($0.status == "abandoned" && $0.startedAt == nil) }
            if taskIDs.contains(where: {
                model.executionsByTaskID[$0] == nil && model.executionsLoadFailureByTaskID[$0] == nil
            }) {
                ProgressView().controlSize(.small)
            }
            if !visible.isEmpty || !abandoned.isEmpty {
                ScrollView {
                    VStack(alignment: .leading, spacing: 2) {
                        ForEach(visible) { execution in
                            Button {
                                openWindow(id: "transcript-viewer", value: TranscriptViewerRef(
                                    taskId: execution.workItemId, preselectExecutionId: execution.id
                                ))
                            } label: {
                                ExecutionRow(exec: execution).frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .buttonStyle(.plain)
                        }
                        if !abandoned.isEmpty {
                            Button(showAllAbandonedAttempts ? "Hide abandoned attempts"
                                   : "\(abandoned.count) abandoned attempts") {
                                showAllAbandonedAttempts.toggle()
                            }
                            .buttonStyle(.link)
                            .font(.caption)
                        }
                    }
                }
                .frame(maxHeight: 240)
            } else if taskIDs.allSatisfy({ model.executionsByTaskID[$0] != nil }) {
                Text("No executions yet.").font(.caption).foregroundStyle(.tertiary)
            }
        }
        .onAppear { updateConsumer(consumer) }
        .onChange(of: taskIDs) { _, _ in updateConsumer(consumer) }
        .onChange(of: isActive) { _, _ in updateConsumer(consumer) }
        .onDisappear { consumer.update(taskIDs: [], model: model) }
        .onChange(of: executionsLoadedCounts) { _, _ in
            guard isActive else { return }
            for id in taskIDs { ExecutionHistoryTiming.record("view_update", taskID: id) }
        }
    }
}

/// Tracks the registered set independently of the current family snapshot.
@MainActor
final class WorkExecutionsConsumer {
    private let token = UUID()
    private(set) var registered: Set<String> = []

    func update(taskIDs: Set<String>, model: ChatViewModel, live: Bool = true) {
        for id in registered.subtracting(taskIDs) {
            model.endExecutionsConsumer(taskID: id, token: token)
        }
        let added = taskIDs.subtracting(registered)
        for id in added {
            model.beginExecutionsConsumer(taskID: id, token: token, live: live)
        }
        for id in taskIDs.subtracting(added) {
            model.setExecutionsConsumerLive(taskID: id, token: token, live: live)
        }
        registered = taskIDs
    }
}
