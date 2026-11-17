import SwiftUI

enum NavigatorActivityVariant: Equatable {
    case working
    case starting
    case failed
    case backlog
    case landed
}

struct WorkNavigatorActivity: View {
    let context: NavigatorDetailContext
    var workspace: WorkersWorkspaceModel?
    @Environment(\.openWindow) private var openWindow

    private var model: ChatViewModel { context.model }
    private var root: WorkTask { context.root }
    private var projection: NavigatorRoot? { root.navigator?.root }
    private var selected: NavigatorActivity? { model.navigatorSelectedActivity(for: root) }
    private var liveActivities: [NavigatorActivity] { projection?.activities ?? [] }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if let selected, !liveActivities.isEmpty || model.navigator.activityByRoot[root.id] != nil {
                activityPicker(selected)
            }
            if variant == .working {
                workingBody
            } else {
                restingBody
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .accessibilityIdentifier("navigator-activity")
    }

    var variant: NavigatorActivityVariant {
        let wait = projection?.reason.wait?.kind
        if projection?.knownSection == .landed { return .landed }
        if wait == "failed" || wait == "interrupted" || root.dispatchFailedReason != nil {
            return .failed
        }
        if projection?.knownSection == .working, !liveActivities.isEmpty { return .working }
        if wait == "starting" { return .starting }
        return .backlog
    }

    var isLiveSelected: Bool {
        guard let selected, liveActivities.contains(where: { $0.executionId == selected.executionId }) else {
            return false
        }
        let runId = selected.runId ?? selected.executionId
        guard let live = model.liveWorkerStates.byRunID[runId] else { return false }
        switch live.activity {
        case .spawning, .working, .waitingForInput, .idle: return true
        case .errored, .terminated: return false
        }
    }

    /// Sticky picker choice that has left the live activity list.
    var selectedExecutionEnded: Bool {
        guard let selected else { return false }
        return !liveActivities.contains(where: { $0.executionId == selected.executionId })
    }

    @ViewBuilder
    private func activityPicker(_ selected: NavigatorActivity) -> some View {
        HStack(spacing: 8) {
            Picker("Current activity", selection: Binding(
                get: { selected.executionId },
                set: { model.setNavigatorActivity(executionID: $0, rootID: root.id) }
            )) {
                ForEach(pickerActivities, id: \.executionId) { activity in
                    Text(activityLabel(activity)).tag(activity.executionId)
                }
            }
            .labelsHidden()
            .frame(maxWidth: 360)
            .accessibilityIdentifier("navigator-activity-picker")
            if selectedExecutionEnded {
                Text("Execution ended; new revision available")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("navigator-execution-ended")
            }
        }
    }

    private var pickerActivities: [NavigatorActivity] {
        var items = liveActivities
        if let selected, !items.contains(where: { $0.executionId == selected.executionId }) {
            items.insert(selected, at: 0)
        }
        return items
    }

    private func activityLabel(_ activity: NavigatorActivity) -> String {
        let taskName = model.task(withID: activity.taskId)?.name ?? activity.taskId
        let live = model.liveWorkerStates.byRunID[activity.runId ?? activity.executionId]
        if let name = live?.displayName {
            return "\(taskName) · \(name)"
        }
        return taskName
    }

    private var workingBody: some View {
        VSplitView {
            WorkNavigatorBrief(context: context)
                .frame(minHeight: 120)
            terminalRegion
                .frame(minHeight: 140)
        }
    }

    @ViewBuilder
    private var restingBody: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                reasonBanner
                if variant == .failed { failureBody }
                if variant == .starting {
                    Label("Agent pane not available yet.", systemImage: "hourglass")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                if variant == .landed {
                    landedBody
                }
                WorkNavigatorBrief(context: context)
                    .frame(minHeight: 160)
                WorkDependenciesSection(model: model, taskID: root.id) { row in
                    model.navigateNavigatorDependency(row)
                }
                commandRow
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    @ViewBuilder
    private var reasonBanner: some View {
        if let label = projection?.reasonLabel {
            Label(label, systemImage: variant == .failed ? "exclamationmark.triangle" : "pause.circle")
                .font(.callout)
                .foregroundStyle(variant == .failed ? Color.orange : Color.secondary)
        }
        if let wait = projection?.reason.wait {
            if let at = wait.at {
                Text("Not before \(at)").font(.caption).foregroundStyle(.secondary)
            }
            if let blocker = wait.blocker, let task = model.task(withID: blocker) {
                Button("Blocked by \(task.name)") {
                    model.navigateNavigatorDependency(WorkDependencyRow(
                        id: task.id, title: task.name, status: task.status, kind: .task
                    ))
                }
                .buttonStyle(.link)
            }
        }
    }

    @ViewBuilder
    private var failureBody: some View {
        if let reason = root.dispatchFailedReason {
            WorkDispatchFailureBanner(reason: reason, errorText: root.dispatchFailedError)
        }
        if let runtime = model.taskRuntimesByID[root.id], !runtime.dispatchFailures.isEmpty,
           let last = runtime.dispatchFailures.last {
            Text(last.errorText).font(.caption).foregroundStyle(.secondary)
        }
        HStack {
            if (projection?.commands.contains { $0.kind == "retry" || $0.kind == "start" }) == true {
                Button("Retry") {
                    model.engine.sendRequestExecution(workItemId: root.id)
                }
                .disabled(context.isReadOnly)
            }
            Button("Open last transcript") {
                openWindow(id: "transcript-viewer", value: TranscriptViewerRef(taskId: root.id))
            }
            .buttonStyle(.link)
        }
    }

    @ViewBuilder
    private var landedBody: some View {
        if let evidence = projection?.mergeEvidence {
            Text(evidence.mergedAt.map { "Merged \($0)" } ?? "Merge observed \(evidence.observedAt)")
                .font(.callout)
        }
        Text("This attempt is complete. File a follow-up for new work.")
            .font(.caption)
            .foregroundStyle(.secondary)
    }

    @ViewBuilder
    private var commandRow: some View {
        let commands = (projection?.commands ?? []).filter { ["start", "retry", "park", "edit_brief"].contains($0.kind) }
        if !commands.isEmpty {
            HStack {
                ForEach(commands, id: \.kind) { command in
                    Button(command.kind == "edit_brief" ? "Edit brief" : command.kind.capitalized) {
                        model.performNavigatorCommand(command)
                    }
                    .disabled(context.isReadOnly)
                }
            }
        }
    }

    @ViewBuilder
    private var terminalRegion: some View {
        if variant == .landed || variant == .failed || variant == .backlog {
            EmptyView()
        } else if variant == .starting, session == nil {
            startingPlaceholder
        } else if let session, session.hostView != nil {
            VStack(alignment: .leading, spacing: 4) {
                if !isLiveSelected {
                    Text("Showing the last session. It is not a live agent.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                EmbeddedWorkerTerminalView(
                    session: session,
                    isActive: model.navigationMode == .work
                        && model.navigator.mode == .navigator
                        && model.navigator.tab == .activity
                )
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .accessibilityIdentifier("navigator-terminal")
            }
        } else {
            startingPlaceholder
        }
    }

    private var startingPlaceholder: some View {
        Label("Agent pane not available yet.", systemImage: "terminal")
            .font(.callout)
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
            .background(Color(nsColor: .black).opacity(0.85))
            .accessibilityIdentifier("navigator-terminal-unavailable")
    }

    private var session: TerminalPaneSession? {
        guard let selected else { return nil }
        let runId = selected.runId ?? selected.executionId
        return workspace?.viewer(forRunId: runId)?.session
    }
}

extension NavigatorDetailRegistry {
    static func navigator(workspace: WorkersWorkspaceModel?) -> NavigatorDetailRegistry {
        var registry = NavigatorDetailRegistry()
        registry.register(.activity) { context in
            AnyView(WorkNavigatorActivity(context: context, workspace: workspace))
        }
        registry.register(.history) { context in
            AnyView(WorkNavigatorHistory(context: context))
        }
        WorkNavigatorAttention.register(in: &registry)
        return registry
    }
}
