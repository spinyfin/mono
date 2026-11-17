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
            if let notice = endedNotice {
                Text(notice)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("navigator-execution-ended")
            }
        }
    }

    /// "New revision available" only when a different live activity has
    /// replaced the one in view; resting states just report that it ended.
    var endedNotice: String? {
        guard selectedExecutionEnded, let selected else { return nil }
        let replaced = variant == .working
            && liveActivities.contains(where: { $0.executionId != selected.executionId })
        return replaced ? "Execution ended; new revision available" : "Execution ended"
    }

    /// The selected run is over, either gone from the live list or reported
    /// terminated/errored by the live worker state.
    var selectedRunEnded: Bool {
        if selectedExecutionEnded { return true }
        guard let selected, let live = model.liveWorkerStates.byRunID[selected.runId ?? selected.executionId] else {
            return false
        }
        switch live.activity {
        case .errored, .terminated: return true
        case .spawning, .working, .waitingForInput, .idle: return false
        }
    }

    /// The family member whose run failed. The picker's sticky selection is
    /// deliberately not consulted: the current revision wins when it has a
    /// failure, then the selected activity's task, then the root.
    var failingTask: WorkTask {
        let current = projection?.revisions.currentTaskId.flatMap { model.task(withID: $0) }
        let candidates = [current, selected.flatMap { model.task(withID: $0.taskId) }, root]
        return candidates.compactMap { $0 }.first(where: hasFailure) ?? current ?? root
    }

    private func hasFailure(_ task: WorkTask) -> Bool {
        task.dispatchFailedReason != nil
            || model.taskRuntimesByID[task.id]?.dispatchFailures.isEmpty == false
    }

    /// Latest recorded dispatch failure text for the failing member, falling
    /// back to the root's when the member's list is empty.
    var failureText: String? {
        let failing = failingTask
        for id in [failing.id, root.id] {
            if let last = model.taskRuntimesByID[id]?.dispatchFailures.last { return last.errorText }
        }
        return nil
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
        let failing = failingTask
        let banner = failing.dispatchFailedReason != nil ? failing : root
        if let reason = banner.dispatchFailedReason {
            WorkDispatchFailureBanner(reason: reason, errorText: banner.dispatchFailedError)
        }
        if let text = failureText {
            Text(text).font(.caption).foregroundStyle(.secondary)
        }
        HStack {
            Button("Open last transcript") {
                openWindow(id: "transcript-viewer", value: TranscriptViewerRef(
                    taskId: failing.id,
                    preselectExecutionId: selected?.taskId == failing.id ? selected?.executionId : nil
                ))
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
                ForEach(commands, id: \.self) { command in
                    Button(command.kind == "edit_brief" ? "Edit brief" : command.kind.capitalized) {
                        model.performNavigatorCommand(command)
                    }
                    .disabled(context.isReadOnly)
                }
            }
        }
    }

    var isSlotActive: Bool {
        model.navigationMode == .work
            && model.navigator.mode == .navigator
            && model.navigator.tab == .activity
    }

    /// Host label when the selected run's worker lives on another machine.
    /// Remote workers have no local terminal, so no viewer attach applies.
    func remoteHost(for activity: NavigatorActivity) -> String? {
        guard let live = model.liveWorkerStates.byRunID[activity.runId ?? activity.executionId],
              live.isSpokeHosted else { return nil }
        return live.hostId ?? "remote host"
    }

    @ViewBuilder
    private var terminalRegion: some View {
        if let selected, let host = remoteHost(for: selected) {
            WorkNavigatorRemotePane(
                model: model.remotePaneViewer,
                runId: selected.runId ?? selected.executionId,
                host: host,
                ended: selectedRunEnded,
                isActive: isSlotActive
            )
        } else if let selected, let workspace {
            WorkNavigatorTerminalSlot(
                selected: selected,
                ended: selectedRunEnded,
                isLive: isLiveSelected,
                isActive: isSlotActive,
                workspace: workspace
            )
        } else {
            NavigatorPaneNotice(text: "Agent pane not available yet.", systemImage: "terminal")
        }
    }
}

struct NavigatorPaneNotice: View {
    let text: String
    let systemImage: String

    var body: some View {
        Label(text, systemImage: systemImage)
            .font(.callout)
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
            .background(Color(nsColor: .black).opacity(0.85))
            .accessibilityIdentifier("navigator-terminal-unavailable")
    }
}

/// The embedded agent pane. Observes the workspace so a late attach
/// re-renders it, and applies the Agents view's availability rules: the
/// refusal reason, a user-driven re-attach request, and an ended run that
/// keeps its transcript reachable.
struct WorkNavigatorTerminalSlot: View {
    let selected: NavigatorActivity
    let ended: Bool
    let isLive: Bool
    let isActive: Bool
    @ObservedObject var workspace: WorkersWorkspaceModel
    @Environment(\.openWindow) private var openWindow

    enum Presentation: Equatable {
        case terminal
        case ended
        case attachFailure(String)
        case unavailable
    }

    /// Which branch the slot renders. A viewer whose session has no host view
    /// yet (a non-PTY or still-launching attach) is not a terminal.
    static func presentation(hasHostedSession: Bool, ended: Bool, failure: String?) -> Presentation {
        if hasHostedSession { return .terminal }
        if ended { return .ended }
        if let failure { return .attachFailure(failure) }
        return .unavailable
    }

    /// A user-driven attach is only asked for while the slot is in view, the
    /// run is live, and no viewer exists yet.
    static func shouldRequestAttach(isActive: Bool, isLive: Bool, ended: Bool, hasSession: Bool) -> Bool {
        isActive && isLive && !ended && !hasSession
    }

    private var runId: String { selected.runId ?? selected.executionId }
    private var session: TerminalPaneSession? { workspace.viewer(forRunId: runId)?.session }

    var body: some View {
        let presentation = Self.presentation(
            hasHostedSession: session?.hostView != nil, ended: ended,
            failure: workspace.viewerAttachFailuresByRunId[runId]
        )
        return Group {
            if presentation == .terminal, let session {
                VStack(alignment: .leading, spacing: 4) {
                    if !isLive {
                        Text("Showing the last session. It is not a live agent.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    EmbeddedWorkerTerminalView(session: session, isActive: isActive)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                        .accessibilityIdentifier("navigator-terminal")
                }
            } else if presentation == .ended {
                VStack(spacing: 8) {
                    Label("Execution ended.", systemImage: "stop.circle")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    Button("Open transcript") {
                        openWindow(id: "transcript-viewer", value: TranscriptViewerRef(
                            taskId: selected.taskId, preselectExecutionId: selected.executionId
                        ))
                    }
                    .buttonStyle(.link)
                    .accessibilityIdentifier("navigator-open-transcript")
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if case .attachFailure(let reason) = presentation {
                NavigatorPaneNotice(text: "Agent pane could not attach: \(reason)", systemImage: "exclamationmark.triangle")
            } else {
                NavigatorPaneNotice(text: "Agent pane not available yet.", systemImage: "terminal")
            }
        }
        .onAppear(perform: requestAttachIfNeeded)
        .onDisappear { workspace.endViewerAttachRequest(forRun: runId) }
        .onChange(of: isActive) { _, active in
            if active { requestAttachIfNeeded() } else { workspace.endViewerAttachRequest(forRun: runId) }
        }
        .onChange(of: runId) { old, _ in
            workspace.endViewerAttachRequest(forRun: old)
            requestAttachIfNeeded()
        }
    }

    private func requestAttachIfNeeded() {
        guard Self.shouldRequestAttach(isActive: isActive, isLive: isLive, ended: ended, hasSession: session != nil)
        else { return }
        workspace.requestViewerAttach(forRuns: [runId])
    }
}

/// Read-only view of a worker hosted on another machine, through the same
/// remote-pane model the Agents view uses. Watching follows visibility.
struct WorkNavigatorRemotePane: View {
    @ObservedObject var model: RemotePaneViewerModel
    let runId: String
    let host: String
    let ended: Bool
    let isActive: Bool

    var body: some View {
        Group {
            if ended {
                NavigatorPaneNotice(text: "Remote execution on \(host) ended.", systemImage: "stop.circle")
            } else {
                RemotePaneViewerView(model: model, runId: runId, hostFallback: host)
                    .accessibilityIdentifier("navigator-remote-pane")
            }
        }
        .onChange(of: watchTarget, initial: true) { _, run in model.watch(run) }
        .onDisappear { model.stopWatching() }
    }

    private var watchTarget: String? { isActive && !ended ? runId : nil }
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
