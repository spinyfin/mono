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
    let onRevealWorkItem: (String, String) -> Void

    @StateObject private var grid = AgentsGridModel()
    @State private var emptyFlavorCycle = Int.random(in: 0...10_000)

    init(
        workspace: WorkersWorkspaceModel,
        liveStates: LiveWorkerStateStore,
        isVisible: Bool,
        onRevealWorkItem: @escaping (String, String) -> Void = { _, _ in }
    ) {
        self.workspace = workspace
        self.liveStates = liveStates
        self.isVisible = isVisible
        self.onRevealWorkItem = onRevealWorkItem
    }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.workspace === rhs.workspace
            && lhs.liveStates === rhs.liveStates
            && lhs.isVisible == rhs.isVisible
    }

    var body: some View {
        VStack(spacing: 0) {
            agentsHeader
            Divider()
            paneArea
        }
        .background(Color(nsColor: .separatorColor))
        .onAppear(perform: bindAndSync)
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
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.willStartLiveResizeNotification)) { _ in
            grid.liveResizeWillStart()
        }
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.didEndLiveResizeNotification)) { _ in
            grid.liveResizeDidEnd()
        }
    }

    private var agentsHeader: some View {
        AgentsToolbar(grid: grid, feedUnavailable: liveStates.feedUnavailable)
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
                        isHighlighted: grid.highlightedRunId == runId,
                        focusCoordinator: workspace.agentFocus,
                        focusRunId: runId,
                        onRevealWorkItem: onRevealWorkItem
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
        grid.renderSnapshot(
            runId: runId,
            workspace: workspace,
            liveStates: liveStates
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
            Self.applyMembership(grid: grid, workspace: workspace, liveStates: liveStates, isVisible: true)
            return grid.reveal(runId: runId)
        }
    }

    /// Feeds every input change to the grid, visible or hidden. Hidden
    /// changes apply immediately under the layout's hidden rule; the only
    /// thing visibility gates is what is drawn.
    private func syncGrid() {
        Self.applyMembership(grid: grid, workspace: workspace, liveStates: liveStates, isVisible: isVisible)
    }

    private static func applyMembership(
        grid: AgentsGridModel,
        workspace: WorkersWorkspaceModel,
        liveStates: LiveWorkerStateStore,
        isVisible: Bool
    ) {
        grid.sync(isVisible: isVisible, workspace: workspace, liveStates: liveStates)
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
    var onRevealWorkItem: ((String, String) -> Void)? = nil

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

    private var slotBody: some View {
        WorkerSlotBody(
            snapshot: snapshot,
            terminal: { terminal($0) },
            placeholder: { placeholderCard },
            viewerMissing: { viewerMissingCard }
        )
    }

    private func terminal(_ session: TerminalPaneSession) -> some View {
        WorkerPaneTerminalView(
            runtime: runtime,
            session: session,
            liveStatePresent: snapshot.live != nil
        )
        .onChange(of: snapshot.displayName, initial: true) { _, name in
            session.setWorkerName(name)
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
            if let live = snapshot.live, let label = snapshot.statusPillLabel {
                statusPill(label, color: liveActivityColor(live))
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

                if let links = snapshot.headerWorkLinks {
                    slotWorkLinks(links)
                }
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
                // Normal work needs no extra badge beside the agent kind.
                if snapshot.showsStatusPill {
                    statusPill(label, color: snapshot.isPlaceholder ? .secondary : liveActivityColor(snapshot.live))
                }
            } else if let session = snapshot.session {
                WorkerSlotFallbackMonitorPill(session: session)
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, snapshot.headerWorkLinks == nil ? 6 : 4)
        .help(slotTooltip)
    }

    /// Compact third header line: `T<n>` · `repo#n`. Truncates as a
    /// single line. Omitted entirely when the pane has no work item.
    @ViewBuilder
    private func slotWorkLinks(_ links: WorkerHeaderWorkLinks) -> some View {
        HStack(spacing: 4) {
            Button {
                if let productId = links.productId {
                    onRevealWorkItem?(links.workItemId, productId)
                }
            } label: {
                Text(links.shortIdLabel)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            .buttonStyle(.plain)
            .pointerStyle(.link)
            .disabled(links.productId == nil)
            .help(links.title)
            .accessibilityLabel("ID \(links.shortIdLabel)")

            if let prURL = links.prURL, let prLabel = links.prLabel {
                Text("·")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                if let url = URL(string: prURL) {
                    Link(destination: url) {
                        Text(prLabel)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    .buttonStyle(.plain)
                    .pointerStyle(.link)
                    .help(prURL)
                    .accessibilityLabel(prLabel)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func liveActivityColor(_ live: WorkerSlotLiveSlice?) -> Color {
        if live?.hungSince != nil { return .orange }
        if live?.activity == .idle, live?.isWaitingOnJob == true { return .secondary }
        switch live?.activity {
        case .working: return .blue
        case .waitingForInput: return .orange
        case .waitingOnWork: return .teal
        case .waitOverdue: return .yellow
        case .idle: return .green
        case .spawning: return .secondary
        case .errored: return .red
        case .terminated, nil: return .secondary
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
        if snapshot.live?.activity == .terminated {
            Text("Exited")
                .font(.caption2)
                .foregroundStyle(.secondary)
        } else if snapshot.live?.hungSince != nil {
            Label("Hung — inspect or restart worker", systemImage: "hourglass")
                .font(.caption2)
                .foregroundStyle(.orange)
        } else if let recovering = snapshot.live?.recoveryStatus,
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
        } else if let wait = snapshot.live?.waitCaption {
            HStack(alignment: .firstTextBaseline, spacing: 4) {
                Image(systemName: "clock")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                Text(wait)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .help(snapshot.runId ?? "")
                    .accessibilityLabel(wait)
            }
        } else if let live = snapshot.live?.activity.slotSubtitle(prior: snapshot.live?.liveStatus),
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

    private var label: String { type.badgeLabel }

    var body: some View {
        Text(type == .unknown && !(raw ?? "").isEmpty ? "Unknown: \(raw ?? "")" : label)
            .font(.caption2.weight(.semibold))
            .lineLimit(1)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(color.opacity(0.16))
            .foregroundStyle(color)
            .clipShape(RoundedRectangle(cornerRadius: 4, style: .continuous))
            .help(helpText)
            .accessibilityLabel(type == .unknown ? "Unknown agent type \(raw ?? "")" : label)
    }

    private var color: Color {
        switch type {
        case .coding: .blue
        case .design: .purple
        case .review, .guide, .consolidation: .orange
        case .automation: .teal
        case .answer: .green
        case .unknown: .red
        }
    }

    private var helpText: String {
        if type == .unknown, let raw, !raw.isEmpty {
            return "Unknown agent type: \(raw)"
        }
        return label
    }
}

/// Fallback titlebar pill driven by the session's pane-monitor scrape.
/// Observes the session so it still updates while `WorkerSlotView` is
/// skipped by `.equatable()`. Only mounted when no `LiveWorkerState`
/// has arrived yet (the pre-hook window).
private struct WorkerSlotFallbackMonitorPill: View {
    @ObservedObject var session: TerminalPaneSession

    var body: some View {
        if session.paneMonitorState != .working {
            statusPill(
                session.paneMonitorState.label,
                color: paneMonitorStateColor(session.paneMonitorState)
            )
        }
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
