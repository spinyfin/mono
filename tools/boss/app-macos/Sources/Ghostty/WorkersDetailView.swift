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
///
/// Pane state is derived, never cached: each body pass builds every slot's
/// `WorkerSlotSnapshot` from the current layout cell, viewer map and
/// live-state store (`WorkerSlotSnapshot.render`). A run the engine has
/// released is therefore the finished placeholder — or gone — the moment
/// the inputs say so, whether or not Agents was visible when the release,
/// detach, or slot reuse arrived. `.equatable()` on `WorkerSlotView` still
/// skips unchanged slot bodies because the snapshot is a value.
struct WorkersDetailView: View, @MainActor Equatable {
    @ObservedObject var workspace: WorkersWorkspaceModel
    @ObservedObject var liveStates: LiveWorkerStateStore
    let isVisible: Bool
    let liveStatusDisabledSlotIDs: Set<Int>
    let onToggleLiveStatus: (Int, Bool) -> Void

    @StateObject private var grid = AgentsGridModel()
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
        .onChange(of: isVisible) { _, _ in syncGrid() }
        .onChange(of: liveStates.byRunID) { _, _ in syncGrid() }
        .onChange(of: liveStates.feedUnavailable) { _, _ in syncGrid() }
        .onChange(of: workspace.viewersByRunId) { _, _ in syncGrid() }
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
            Spacer(minLength: 8)
            if grid.layout.filter.isActive {
                hiddenSummary
            }
            if grid.layout.waitingCount > 0 {
                Text("Needs input \(grid.layout.waitingCount)")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(.orange)
            }
            Button("Tidy") { grid.tidy() }
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
                Button("Clear") { grid.clearFilter() }
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
        HStack(spacing: 6) {
            Text("Page:")
                .font(.caption)
                .foregroundStyle(.secondary)
            ForEach(grid.layout.pageSelectors, id: \.index) { selector in
                AgentPageSelectorChip(
                    selector: selector,
                    isSelected: selector.index == grid.layout.selectedPage
                ) {
                    grid.selectPage(selector.index)
                }
            }
        }
    }

    // A single flat collection owns surfaces for their entire attached lifetime.
    private var paneArea: some View {
        let layout = grid.layout
        return GeometryReader { geo in
            ZStack(alignment: .topLeading) {
                AgentSurfaceCollection(runIds: grid.renderRunIds(attached: workspace.viewersByRunId.keys), layout: layout, area: geo.size) { runId in
                    let snapshot = renderSnapshot(runId: runId)
                    WorkerSlotView(
                        runtime: workspace.runtime,
                        snapshot: snapshot,
                        onToggleLiveStatus: { enabled in
                            onToggleLiveStatus(snapshot.slotId, enabled)
                        }
                    )
                    .equatable()
                }
                if grid.layout.isEmpty { emptyState }
            }
        }
        .background(paneSizeReader)
    }

    /// Current render state for one mounted surface. The engine's live
    /// state decides live versus finished; the layout only decides where
    /// (and whether) the surface is visible.
    private func renderSnapshot(runId: String) -> WorkerSlotSnapshot {
        WorkerSlotSnapshot.render(
            runId: runId,
            viewer: workspace.viewersByRunId[runId],
            liveState: liveStates.byRunID[runId],
            liveStatusDisabledSlotIDs: liveStatusDisabledSlotIDs,
            cachedPersona: grid.personaName(for: runId)
        )
    }

    private var emptyState: some View {
        let crew = TrekPortraitGrid.workerNames
        let character = crew[abs(emptyFlavorCycle) % max(crew.count, 1)]
        return VStack(spacing: 14) {
            Spacer()
            if let nsImage = TrekIconAssets.portrait(forPersona: character) {
                Image(nsImage: nsImage)
                    .resizable()
                    .interpolation(.none)
                    .aspectRatio(contentMode: .fit)
                    .frame(maxWidth: 220, maxHeight: 240)
                    .opacity(0.85)
            }
            Text(grid.layout.filter.isActive && grid.layout.hiddenCount > 0
                ? "No agents match the filter" : "No agents running")
                .font(.title3.weight(.semibold))
                .foregroundStyle(Color.white.opacity(0.85))
            Text(TrekIdleFlavor.line(for: character, cycle: emptyFlavorCycle))
                .font(.callout)
                .foregroundStyle(Color.white.opacity(0.6))
                .multilineTextAlignment(.center)
                .padding(.horizontal, 24)
                .lineLimit(3)
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .black))
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
        syncGrid()
    }

    /// Feeds every input change to the grid, visible or hidden. Hidden
    /// changes apply immediately under the layout's hidden rule; the only
    /// thing visibility gates is what is drawn.
    private func syncGrid() {
        grid.apply(
            isVisible: isVisible,
            states: Array(liveStates.byRunID.values),
            viewers: workspace.viewersByRunId,
            interactivePoolSize: workspace.interactivePoolSize,
            automationPoolSize: workspace.automationPoolSize,
            reviewPoolSize: workspace.reviewSlotCount,
            feedUnavailable: liveStates.feedUnavailable
        )
    }
}

private struct AgentPageSelectorChip: View {
    let selector: PageSelector
    let isSelected: Bool
    let onSelect: () -> Void

    var body: some View {
        Button(action: onSelect) {
            HStack(spacing: 4) {
                Text(pageLabel)
                    .font(.caption.weight(.semibold))
                Text("·")
                    .foregroundStyle(.secondary)
                Text(occupiedLabel)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if selector.hasWaiting {
                    Circle()
                        .fill(Color.orange)
                        .frame(width: 6, height: 6)
                        .accessibilityLabel("Waiting workers")
                }
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(
                isSelected
                    ? Color.accentColor.opacity(0.18)
                    : Color.secondary.opacity(0.08)
            )
            .clipShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Page \(selector.index + 1), \(selector.occupiedCount) agents")
    }

    private var pageLabel: String { String(selector.index + 1) }
    private var occupiedLabel: String {
        "\(selector.occupiedCount) \(selector.occupiedCount == 1 ? "Agent" : "Agents")"
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
    let onToggleLiveStatus: (Bool) -> Void

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.runtime === rhs.runtime
            && lhs.snapshot == rhs.snapshot
    }

    var body: some View {
        VStack(spacing: 0) {
            slotHeader
                .opacity(snapshot.isPlaceholder || snapshot.isExited ? 0.55 : 1)
            Divider()
            slotBody
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    @ViewBuilder
    private var slotBody: some View {
        switch snapshot.bodyKind {
        case .placeholder:
            placeholderCard
        case .terminal:
            if let session = snapshot.session {
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
            }
        case .viewerMissing:
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
            if let badge = snapshot.headerBadge {
                AgentTypeBadge(type: badge, raw: snapshot.live?.agentType)
            }
            if let live = snapshot.live {
                statusPill(live.activity.agentsPaneLabel, color: liveActivityColor(live.activity))
            }
            Text("Viewer not attached")
                .font(.callout)
                .foregroundStyle(Color.white.opacity(0.6))
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .black))
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

            if let badge = snapshot.headerBadge {
                AgentTypeBadge(type: badge, raw: snapshot.live?.agentType)
            }

            // Prefer engine-supplied LiveWorkerState — its activity is
            // driven by hook events rather than a screen-scrape that
            // always rendered "Agent Unknown". Fall back to a session-
            // observing pill until the worker's first hook fires, so
            // `.equatable()` skipping this body does not freeze the
            // fallback chrome.
            if let label = snapshot.statusPillLabel {
                statusPill(label, color: snapshot.isPlaceholder ? .secondary : liveActivityColor(snapshot.live?.activity))
            } else if let session = snapshot.session {
                WorkerSlotFallbackMonitorPill(session: session)
            }

            if snapshot.showsLiveStatusToggle {
                liveStatusToggle
            }
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

    private func liveActivityColor(_ activity: WorkerActivity?) -> Color {
        switch activity {
        case .working: .blue
        case .waitingForInput: .orange
        case .idle: .green
        case .spawning: .secondary
        case .errored: .red
        case .terminated, nil: .secondary
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
        Text(snapshot.headerTitle)
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
