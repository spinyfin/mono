import AppKit
import PaneLayout
import SwiftUI

/// Agents-tab shell. Observes the workspace (viewer map) and the live-state
/// store (engine membership) — the two objects whose publishes change pane
/// chrome — and receives live-status flags and tab visibility as values from
/// `ContentView`. It does **not** observe `ChatViewModel`. Closures stay
/// outside `==` so a parent rebuild of the toggle handler cannot defeat
/// `.equatable()` at the call site.
///
/// Every attached run stays in the `ZStack` so pagination is a display
/// filter — no `dismantleNSView`, no libghostty surface teardown. Off-page
/// and unplaced attached surfaces stay mounted at zero opacity.
struct WorkersDetailView: View, @MainActor Equatable {
    @ObservedObject var workspace: WorkersWorkspaceModel
    @ObservedObject var liveStates: LiveWorkerStateStore
    let isVisible: Bool
    let liveStatusDisabledSlotIDs: Set<Int>
    let onToggleLiveStatus: (Int, Bool) -> Void

    @StateObject private var grid = AgentsGridModel()
    @State private var cachedSnapshots: [String: WorkerSlotSnapshot] = [:]
    @State private var emptyFlavorCycle = Int.random(in: 0...10_000)

    init(
        workspace: WorkersWorkspaceModel,
        liveStates: LiveWorkerStateStore,
        isVisible: Bool,
        liveStatusDisabledSlotIDs: Set<Int>,
        onToggleLiveStatus: @escaping (Int, Bool) -> Void
    ) {
        self.workspace = workspace
        self.liveStates = liveStates
        self.isVisible = isVisible
        self.liveStatusDisabledSlotIDs = liveStatusDisabledSlotIDs
        self.onToggleLiveStatus = onToggleLiveStatus
    }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.workspace === rhs.workspace
            && lhs.liveStates === rhs.liveStates
            && lhs.isVisible == rhs.isVisible
            && lhs.liveStatusDisabledSlotIDs == rhs.liveStatusDisabledSlotIDs
    }

    var body: some View {
        VStack(spacing: 0) {
            agentsHeader
            Divider()
            paneArea
        }
        .background(Color(nsColor: .separatorColor))
        .onAppear(perform: bindAndSync)
        .onChange(of: grid.layout) { _, _ in if isVisible { refreshSnapshots() } }
        .onChange(of: isVisible) { _, visible in
            if !visible { workspace.resetViewerAttachRequests() }
            syncGrid()
        }
        .onChange(of: liveStates.byRunID) { _, _ in syncGrid() }
        .onChange(of: liveStates.feedUnavailable) { _, _ in syncGrid() }
        .onChange(of: workspace.viewersByRunId) { _, _ in syncGrid() }
        .onChange(of: workspace.viewerAttachFailuresByRunId) { _, _ in syncGrid() }
        .onChange(of: workspace.interactivePoolSize) { _, _ in syncGrid() }
        .onChange(of: workspace.automationPoolSize) { _, _ in syncGrid() }
        .onChange(of: workspace.reviewSlotCount) { _, _ in syncGrid() }
        .onChange(of: liveStatusDisabledSlotIDs) { _, _ in syncGrid() }
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.willStartLiveResizeNotification)) { _ in
            grid.liveResizeWillStart()
        }
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.didEndLiveResizeNotification)) { _ in
            grid.liveResizeDidEnd()
        }
    }

    private var agentsHeader: some View {
        HStack(spacing: 10) {
            if liveStates.feedUnavailable {
                Label("Showing last known agents", systemImage: "wifi.slash")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .help("The engine feed is disconnected. Membership is the last accepted snapshot.")
            }
            filterMenus
            if grid.layout.selectorsVisible {
                pageSelectors
            }
            if let banner = grid.filterClearedBanner {
                Text(banner)
                    .font(.caption.weight(.medium))
                    .foregroundStyle(Color.accentColor)
                    .transition(.opacity)
            }
            Spacer(minLength: 8)
            if grid.layout.filter.isActive {
                hiddenSummary
            }
            if grid.layout.waitingCount > 0 {
                Text("Needs input \(grid.layout.waitingCount)")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(.orange)
            }
            Button("Tidy") { grid.tidy(); refreshSnapshots() }
                .font(.caption)
                .disabled(!grid.layout.isTidyHighlighted)
                .opacity(grid.layout.isTidyHighlighted ? 1 : 0.45)
                .help("Pack agents into the current sort order and drop empty cells.")
            Text(grid.poolStrip)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private var filterMenus: some View {
        HStack(spacing: 6) {
            Menu {
                ForEach(grid.projectOptions, id: \.self) { project in
                    Toggle(grid.projectLabel(project), isOn: Binding(
                        get: { grid.layout.filter.projects?.contains(project) ?? false },
                        set: { _ in grid.toggleProject(project) }
                    ))
                }
            } label: {
                Text(filterTitle("Project", count: grid.layout.filter.projects?.count))
            }
            Menu {
                ForEach(grid.typeOptions, id: \.self) { type in
                    Toggle(type.badgeLabel, isOn: Binding(
                        get: { grid.layout.filter.types?.contains(type) ?? false },
                        set: { _ in grid.toggleType(type) }
                    ))
                }
            } label: {
                Text(filterTitle("Type", count: grid.layout.filter.types?.count))
            }
            if grid.layout.filter.isActive {
                Button("Clear") { grid.clearFilter(); refreshSnapshots() }
                    .help("Show all agents again.")
            }
        }
        .font(.caption)
        .menuStyle(.borderlessButton)
        .fixedSize()
    }

    private func filterTitle(_ name: String, count: Int?) -> String {
        guard let count else { return "\(name): All" }
        return "\(name): \(count)"
    }

    /// Hidden agents keep running and streaming; a hidden agent that needs
    /// input is called out so a filter cannot bury it.
    private var hiddenSummary: some View {
        HStack(spacing: 6) {
            Text("\(grid.layout.hiddenCount) hidden")
                .font(.caption)
                .foregroundStyle(.secondary)
            if grid.layout.hiddenWaitingCount > 0 {
                Text("\(grid.layout.hiddenWaitingCount) need input")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(.orange)
            }
        }
        .help("Filtered-out agents are still running; filtering only changes what is shown.")
    }

    private var pageSelectors: some View {
        AgentPageSelectors(layout: grid.layout, onSelect: grid.selectPage)
    }

    // A single flat collection owns surfaces for their entire attached lifetime.
    private var paneArea: some View {
        let layout = grid.layout
        return GeometryReader { geo in
            ZStack(alignment: .topLeading) {
                AgentSurfaceCollection(runIds: renderRunIds, layout: layout, area: geo.size) { runId in
                    let snapshot = renderSnapshot(runId: runId)
                    WorkerSlotView(
                        runtime: workspace.runtime,
                        snapshot: snapshot,
                        isHighlighted: grid.highlightedRunId == runId,
                        focusCoordinator: workspace.agentFocus,
                        focusRunId: runId,
                        onToggleLiveStatus: { enabled in
                            onToggleLiveStatus(snapshot.slotId, enabled)
                        }
                    )
                    .equatable()
                }
                idleCells(area: geo.size)
                if grid.layout.isEmpty { emptyState }
            }
        }
        .background(paneSizeReader)
    }

    private var renderRunIds: [String] {
        Set(cachedSnapshots.keys).union(workspace.viewersByRunId.keys).sorted()
    }

    private func renderSnapshot(runId: String) -> WorkerSlotSnapshot {
        if let cached = cachedSnapshots[runId],
           cached.session === workspace.viewersByRunId[runId]?.session {
            return cached
        }
        return snapshot(runId: runId, isPlaceholder: false)
    }

    private func cellFrame(page: PaneLayoutPage, localIndex: Int, area: CGSize) -> CGRect {
        let width = max(1, (area.width - CGFloat(page.shape.columns - 1)) / CGFloat(page.shape.columns))
        let height = max(1, (area.height - CGFloat(page.shape.rows - 1)) / CGFloat(page.shape.rows))
        let position = page.shape.position(ofLocalIndex: localIndex)
        return CGRect(x: CGFloat(position.column) * (width + 1), y: CGFloat(position.row) * (height + 1), width: width, height: height)
    }

    private var emptyState: some View {
        let crew = TrekPortraitGrid.workerNames
        let character = crew[abs(emptyFlavorCycle) % max(crew.count, 1)]
        return IdleCrewView(persona: character, title: grid.layout.filter.isActive && grid.layout.hiddenCount > 0
                ? "No agents match the filter" : "No agents running", flavorCycle: emptyFlavorCycle)
    }

    /// Decorative fillers for the selected page's unassigned rectangles.
    /// Hit testing is off so they never take focus or clicks from real panes.
    @ViewBuilder
    private func idleCells(area: CGSize) -> some View {
        if !grid.layout.isEmpty, let page = grid.layout.pages.first(where: { $0.index == grid.layout.selectedPage }) {
            ForEach(grid.idleCells(for: page), id: \.localIndex) { idle in
                IdleCrewView(persona: idle.persona, title: idle.persona, flavorCycle: idle.flavorCycle, compact: true)
                    .frame(
                        width: cellFrame(page: page, localIndex: idle.localIndex, area: area).width,
                        height: cellFrame(page: page, localIndex: idle.localIndex, area: area).height
                    )
                    .offset(
                        x: cellFrame(page: page, localIndex: idle.localIndex, area: area).minX,
                        y: cellFrame(page: page, localIndex: idle.localIndex, area: area).minY
                    )
                    .allowsHitTesting(false)
                    .accessibilityHidden(true)
            }
        }
    }

    private var paneSizeReader: some View {
        GeometryReader { geo in
            Color.clear.preference(key: AgentsPaneSizeKey.self, value: geo.size)
        }
        .onPreferenceChange(AgentsPaneSizeKey.self) { size in
            Task { @MainActor in
                let liveResize = NSApp.windows.contains { $0.inLiveResize }
                grid.noteArea(width: size.width, height: size.height, inLiveResize: liveResize)
            }
        }
    }

    private func bindAndSync() {
        if let cell = workspace.runtime.lastCellSize {
            grid.noteCellSize(width: cell.width, height: cell.height)
        }
        workspace.runtime.onCellSizeChanged = { [grid] width, height in
            Task { @MainActor in
                grid.noteCellSize(width: width, height: height)
            }
        }
        workspace.runtime.onWorkerFocus = { [grid] sessionId in
            grid.setFocusedRun(String(sessionId.dropFirst("run-".count)))
        }
        Self.bindAgentReveal(grid: grid, workspace: workspace, liveStates: liveStates)
        grid.onViewerAttachRequested = { [weak workspace] runs in
            workspace?.requestViewerAttach(forRuns: runs)
        }
        syncGrid()
    }

    static func bindAgentReveal(
        grid: AgentsGridModel,
        workspace: WorkersWorkspaceModel,
        liveStates: LiveWorkerStateStore
    ) {
        workspace.agentsRevealHandler = { [grid, weak workspace, liveStates] runId in
            guard let workspace else { return .notMember }
            // Make membership current before validating the request; the
            // successful reveal will then switch navigation to Agents.
            grid.setVisible(true)
            Self.applyMembership(grid: grid, workspace: workspace, liveStates: liveStates)
            return grid.reveal(runId: runId)
        }
    }

    private func syncGrid() {
        if !isVisible {
            grid.setVisible(false)
            return
        }
        Self.applyMembership(grid: grid, workspace: workspace, liveStates: liveStates)
        grid.setVisible(true)
        refreshSnapshots()
    }

    private static func applyMembership(
        grid: AgentsGridModel,
        workspace: WorkersWorkspaceModel,
        liveStates: LiveWorkerStateStore
    ) {
        grid.apply(
            states: Array(liveStates.byRunID.values),
            viewers: workspace.viewersByRunId,
            interactivePoolSize: workspace.interactivePoolSize,
            automationPoolSize: workspace.automationPoolSize,
            reviewPoolSize: workspace.reviewSlotCount,
            feedUnavailable: liveStates.feedUnavailable
        )
    }

    private func refreshSnapshots() {
        var next: [String: WorkerSlotSnapshot] = [:]
        for page in grid.layout.pages {
            for cell in page.cells {
                if let value = snapshot(for: cell), let runId = value.runId { next[runId] = value }
            }
        }
        if cachedSnapshots != next { cachedSnapshots = next }
    }

    private func snapshot(for cell: PaneCell) -> WorkerSlotSnapshot? {
        switch cell.content {
        case .empty:
            return nil
        case .occupied(let member):
            return snapshot(runId: member.runId, isPlaceholder: false)
        case .placeholder(let runId):
            return snapshot(runId: runId, isPlaceholder: true)
        }
    }

    private func snapshot(runId: String, isPlaceholder: Bool) -> WorkerSlotSnapshot {
        let viewer = workspace.viewersByRunId[runId]
        let live = isVisible ? liveStates.byRunID[runId] : nil
        let slotId = viewer?.slotId ?? live?.slotId ?? 0
        let slot = viewer ?? WorkerSlot(
            slotId: slotId,
            runId: runId,
            summary: nil,
            taskTitle: nil
        )
        return WorkerSlotSnapshot.build(
            slot: slot,
            liveState: live,
            liveStatusEnabled: !liveStatusDisabledSlotIDs.contains(slotId),
            isPlaceholder: isPlaceholder,
            cachedPersona: grid.personaName(for: runId),
            viewerAttachFailure: workspace.viewerAttachFailuresByRunId[runId]
        )
    }
}

private struct AgentsPaneSizeKey: PreferenceKey {
    static let defaultValue: CGSize = .zero
    static func reduce(value: inout CGSize, nextValue: () -> CGSize) {
        value = nextValue()
    }
}

/// One worker slot. Equatable over `WorkerSlotSnapshot` so
/// `.equatable()` at the call site skips body evaluation when this
/// slot's own data has not changed. Closures are outside `==`.
///
/// The libghostty `NSViewRepresentable` lives in `WorkerPaneTerminalView`,
/// which observes the session independently: skipping this body's
/// evaluation leaves that representable mounted (no `dismantleNSView`).
struct WorkerSlotView: View, @MainActor Equatable {
    let runtime: GhosttyRuntime
    let snapshot: WorkerSlotSnapshot
    /// Outline shown on the cell a focus request just revealed.
    var isHighlighted = false
    var focusCoordinator: AgentFocusCoordinator?
    var focusRunId: String?
    let onToggleLiveStatus: (Bool) -> Void

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.runtime === rhs.runtime
            && lhs.snapshot == rhs.snapshot
            && lhs.isHighlighted == rhs.isHighlighted
            && lhs.focusCoordinator === rhs.focusCoordinator
            && lhs.focusRunId == rhs.focusRunId
    }

    var body: some View {
        VStack(spacing: 0) {
            slotHeader
                .opacity(snapshot.isPlaceholder || snapshot.isExited ? 0.55 : 1)
            Divider()
            slotBody
        }
        .background(Color(nsColor: .windowBackgroundColor))
        .overlay {
            if isHighlighted {
                Rectangle()
                    .strokeBorder(Color.accentColor, lineWidth: 3)
                    .allowsHitTesting(false)
            }
        }
    }

    @ViewBuilder
    private var slotBody: some View {
        if snapshot.isPlaceholder, snapshot.session == nil {
            placeholderCard
        } else if let session = snapshot.session {
            WorkerPaneTerminalView(
                runtime: runtime,
                session: session,
                liveStatePresent: snapshot.live != nil
            )
            .id(session.id)
            .opacity(snapshot.isPlaceholder || snapshot.isExited ? 0.55 : 1)
            .onChange(of: snapshot.displayName, initial: true) { _, name in
                session.setWorkerName(name)
            }
        } else {
            viewerMissingCard
        }
    }

    private var placeholderCard: some View {
        VStack(spacing: 8) {
            Spacer()
            personaPortrait(large: false)
            Text("\(snapshot.displayName) finished")
                .font(.callout.weight(.medium))
                .foregroundStyle(Color.white.opacity(0.7))
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .black))
        .opacity(0.55)
    }

    private var viewerMissingCard: some View {
        VStack(spacing: 10) {
            Spacer()
            personaPortrait(large: true)
            Text(snapshot.displayName)
                .font(.title3.weight(.semibold))
                .foregroundStyle(Color.white.opacity(0.85))
            AgentTypeBadge(type: snapshot.badgeType, raw: snapshot.live?.agentType)
            if let live = snapshot.live {
                statusPill(live.activity.agentsPaneLabel, color: liveActivityColor(live.activity))
            }
            // "Connecting…" only while nothing is known to have failed; a
            // refused or failed attach shows its reason (see
            // `WorkerSlotSnapshot.viewerMissingMessage`).
            Text(snapshot.viewerMissingMessage)
                .font(.callout)
                .foregroundStyle(snapshot.isViewerConnecting ? Color.white.opacity(0.6) : Color.orange.opacity(0.9))
                .multilineTextAlignment(.center)
                .padding(.horizontal, 16)
                .lineLimit(4)
                .accessibilityLabel(snapshot.isViewerConnecting ? "Connecting to the worker's terminal" : "Terminal not attached: \(snapshot.viewerMissingMessage)")
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .black))
        .background {
            if let focusCoordinator, let focusRunId {
                AgentCardFocusTarget(coordinator: focusCoordinator, runId: focusRunId)
            }
        }
    }

    @ViewBuilder
    private func personaPortrait(large: Bool) -> some View {
        if let nsImage = TrekIconAssets.portrait(forPersona: snapshot.live?.name ?? snapshot.cachedPersona) {
            Image(nsImage: nsImage)
                .resizable()
                .interpolation(.none)
                .aspectRatio(contentMode: .fit)
                .frame(maxWidth: large ? 220 : 96, maxHeight: large ? 240 : 120)
                .opacity(0.85)
        }
    }

    private var slotHeader: some View {
        HStack(spacing: 8) {
            if let nsImage = TrekIconAssets.portrait(forPersona: snapshot.live?.name ?? snapshot.cachedPersona) {
                Image(nsImage: nsImage)
                    .resizable()
                    .interpolation(.none)
                    .aspectRatio(contentMode: .fit)
                    .frame(width: 28, height: 28)
                    .clipShape(RoundedRectangle(cornerRadius: 3, style: .continuous))
            }

            VStack(alignment: .leading, spacing: 1) {
                slotTaskLine

                slotSubtitle
            }

            Spacer(minLength: 0)

            if snapshot.live != nil {
                AgentTypeBadge(type: snapshot.badgeType, raw: snapshot.live?.agentType)
            }

            // Prefer engine-supplied LiveWorkerState — its activity is
            // driven by hook events rather than a screen-scrape that
            // always rendered "Agent Unknown". Fall back to a session-
            // observing pill until the worker's first hook fires, so
            // `.equatable()` skipping this body does not freeze the
            // fallback chrome.
            if snapshot.isPlaceholder {
                statusPill("Finished", color: .secondary)
            } else if let live = snapshot.live {
                statusPill(
                    live.activity.agentsPaneLabel,
                    color: liveActivityColor(live.activity)
                )
            } else if let session = snapshot.session {
                WorkerSlotFallbackMonitorPill(session: session)
            }

            liveStatusToggle
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .help(slotTooltip)
    }

    /// Tiny eye-icon toggle in the slot header that disables the
    /// live-status summarizer for this slot. Off = engine stops
    /// summarising and the UI falls back to pane_summary. Persisted
    /// across engine restarts; the engine echoes the new state back
    /// so reads via subscribe stay in sync. Tooltip explains the
    /// trade-off so a curious user understands what the icon does.
    @ViewBuilder
    private var liveStatusToggle: some View {
        let enabled = snapshot.liveStatusEnabled
        Button {
            onToggleLiveStatus(!enabled)
        } label: {
            Image(systemName: enabled ? "eye" : "eye.slash")
                .font(.caption2)
                .foregroundStyle(enabled ? Color.secondary : Color(nsColor: .tertiaryLabelColor))
        }
        .buttonStyle(.plain)
        .help(
            enabled
                ? "Live status on — engine summarises this worker's transcript."
                : "Live status off — falls back to the static pane summary."
        )
        .accessibilityLabel(enabled ? "Disable live status" : "Enable live status")
    }

    private func liveActivityColor(_ activity: WorkerActivity) -> Color {
        switch activity {
        case .working: .blue
        case .waitingForInput: .orange
        case .idle: .green
        case .spawning: .secondary
        case .errored: .red
        case .terminated: .secondary
        }
    }

    private var slotTooltip: String {
        var parts: [String] = []
        if let runId = snapshot.runId {
            parts.append("run \(runId)")
        }
        parts.append("slot \(snapshot.slotId)")
        if let pool = snapshot.live?.pool, !pool.isEmpty {
            parts.append(pool)
        }
        if let project = snapshot.live?.projectName, !project.isEmpty {
            parts.append(project)
        } else if snapshot.runId != nil {
            parts.append("Unfiled")
        }
        return parts.joined(separator: " · ")
    }

    /// First line in the titlebar — the overall task this worker is
    /// on. When ANTHROPIC_API_KEY is available and Claude generated a
    /// gerund phrase, it renders as `"<Name> is <gerund>"` (e.g.
    /// "Riker is fixing the fencer scraper"). When the key is absent
    /// or summarization failed, the engine sends the raw task title
    /// and this renders as `"<Name>: <title>"` (e.g. "Crusher:
    /// kanban: revision cards render broken") — grammatically correct
    /// and identifying the task without the gerund connector.
    @ViewBuilder
    private var slotTaskLine: some View {
        let name = snapshot.displayName
        let text: String = {
            if snapshot.isPlaceholder {
                return "\(name) finished"
            }
            if let summary = snapshot.summary, !summary.isEmpty {
                // Success path: Claude-generated gerund phrase.
                // Preserved verbatim — do NOT change this branch.
                return "\(name) is \(summary)"
            }
            if let taskTitle = snapshot.taskTitle, !taskTitle.isEmpty {
                // Fallback path: no gerund available (no API key or
                // summarization failed). Use "<Name>: <task>" format.
                return "\(name): \(taskTitle)"
            }
            if snapshot.runId != nil {
                return "\(name) is working"
            }
            return name
        }()
        Text(text)
            .font(.caption.weight(.semibold))
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .help(snapshot.runId ?? "")
    }

    /// Second line in the titlebar — the engine's real-time
    /// live-status sentence (refreshed by the summarizer in
    /// `engine/src/live_status.rs`). When no live status is
    /// available we fall through to the run id and then to "idle"
    /// so the line still anchors the pane visually. The static
    /// pane-summary gerund is rendered on the first line via
    /// `slotTaskLine` and intentionally not duplicated here.
    @ViewBuilder
    private var slotSubtitle: some View {
        if let recovering = snapshot.live?.recoveryStatus,
           !recovering.isEmpty
        {
            // Transient-recovery banner wins outright: it means the
            // worker looks idle but is actually being auto-resumed
            // after a Claude API error, which is exactly the case a
            // stale/generic "idle" subtitle would hide.
            HStack(alignment: .firstTextBaseline, spacing: 4) {
                Image(systemName: "arrow.triangle.2.circlepath")
                    .font(.caption2)
                    .foregroundStyle(.orange)
                Text(recovering)
                    .font(.caption2)
                    .foregroundStyle(.orange)
                    .lineLimit(1)
                    .help(snapshot.runId ?? "")
                    .accessibilityLabel("Recovering: \(recovering)")
            }
        } else if let live = snapshot.live?.liveStatus,
           !live.isEmpty
        {
            HStack(alignment: .firstTextBaseline, spacing: 4) {
                WorkerWaitingIndicator(
                    activity: snapshot.live?.activity,
                    lastEventAt: snapshot.live?.lastEventAt
                )
                Text(live)
                    .font(.caption2)
                    .foregroundStyle(liveStatusColor)
                    .lineLimit(1)
                    .help(snapshot.runId ?? "")
                    .accessibilityLabel("Live status: \(live)")
            }
        } else if let runId = snapshot.runId {
            Text(runId)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .help(runId)
        } else {
            Text("idle")
                .font(.caption2)
                .foregroundStyle(.tertiary)
                .lineLimit(1)
        }
    }

    /// Match the Doing-card colour mapping: red for errored runs,
    /// tertiary for idle, `.secondary` otherwise. `waitingForInput`
    /// is no longer tinted accent-blue — it surfaces the explicit
    /// `WorkerWaitingIndicator` icon + tooltip in `slotSubtitle`
    /// instead, so the meaning is not carried by hue alone.
    private var liveStatusColor: Color {
        switch snapshot.live?.activity {
        case .errored:
            return .red
        case .idle:
            return Color(nsColor: .tertiaryLabelColor)
        default:
            return .secondary
        }
    }
}

struct AgentTypeBadge: View {
    let type: AgentType
    let raw: String?

    var body: some View {
        Text(type == .unknown && !(raw ?? "").isEmpty ? "Unknown: \(raw ?? "")" : type.badgeLabel)
            .font(.caption2.weight(.semibold))
            .lineLimit(1)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(color.opacity(0.16))
            .foregroundStyle(color)
            .clipShape(RoundedRectangle(cornerRadius: 4, style: .continuous))
            .help(helpText)
            .accessibilityLabel(type == .unknown ? "Unknown agent type \(raw ?? "")" : type.badgeLabel)
    }

    private var color: Color {
        switch type {
        case .coding: .blue
        case .design: .purple
        case .review: .orange
        case .automation: .teal
        case .answer: .green
        case .unknown: .red
        }
    }

    private var helpText: String {
        if type == .unknown, let raw, !raw.isEmpty {
            return "Unknown agent type: \(raw)"
        }
        return type.badgeLabel
    }
}

/// Fallback titlebar pill driven by the session's pane-monitor scrape.
/// Observes the session so it still updates while `WorkerSlotView` is
/// skipped by `.equatable()`. Only mounted when no `LiveWorkerState`
/// has arrived yet (the pre-hook window).
private struct WorkerSlotFallbackMonitorPill: View {
    @ObservedObject var session: TerminalPaneSession

    var body: some View {
        statusPill(
            session.paneMonitorState.label,
            color: paneMonitorStateColor(session.paneMonitorState)
        )
    }
}

private func statusPill(_ text: String, color: Color) -> some View {
    Text(text)
        .font(.caption2.weight(.medium))
        .lineLimit(1)
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(color.opacity(0.14))
        .foregroundStyle(color)
        .clipShape(Capsule())
}

private func paneMonitorStateColor(_ state: PaneMonitorState) -> Color {
    switch state {
    case .working: .blue
    case .ready: .green
    case .notDetected: .secondary
    case .unavailable: .orange
    }
}

private struct WorkerPaneTerminalView: View {
    let runtime: GhosttyRuntime
    @ObservedObject var session: TerminalPaneSession
    /// `true` once the engine has pushed a `LiveWorkerState` for this
    /// worker, which makes the titlebar pill hook-driven and the
    /// per-pane 0.5s viewport screen-scrape redundant.
    let liveStatePresent: Bool

    var body: some View {
        // Once the engine pushes a LiveWorkerState for this worker the
        // titlebar pill renders hook-driven activity and the per-pane
        // 0.5s viewport screen-scrape becomes redundant. Gate the
        // monitor so it only runs as the pre-hook fallback. Passing the
        // gate as a plain input (rather than mutating a @Published on
        // the session) keeps the reconcile out of the render pass —
        // including the spawn case where live state is already present
        // by the time this pane mounts (e.g. a re-render after a run
        // resumed).
        GhosttyTerminalView(
            runtime: runtime,
            session: session,
            launchSpec: session.launchSpec,
            paneMonitorEnabled: !liveStatePresent
        )
        .background(Color(nsColor: .black))
    }
}
