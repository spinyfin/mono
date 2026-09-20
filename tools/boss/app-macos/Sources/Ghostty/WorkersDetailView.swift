import AppKit
import SwiftUI

/// Which worker pool page the Agents tab is currently displaying.
///
/// Bridge Crew and Lower Decks are the two pages of the single interactive
/// worker pool (slots 1...8 and 9...16 respectively). They are one engine
/// pool — dispatch just fills Bridge Crew before spilling into Lower Decks.
enum AgentPoolKind: String, CaseIterable, Identifiable {
    case bridgeCrew
    case lowerDecks
    case automations
    case reviewers

    var id: String { rawValue }

    func label(bridgeCrewCount: Int, lowerDecksCount: Int, automationCount: Int, reviewCount: Int) -> String {
        switch self {
        case .bridgeCrew: return "Bridge Crew (\(bridgeCrewCount))"
        case .lowerDecks: return "Lower Decks (\(lowerDecksCount))"
        case .automations: return "Automations (\(automationCount))"
        case .reviewers: return "Reviewers (\(reviewCount))"
        }
    }
}

/// Agents-tab shell. Observes the workspace and the live-state store —
/// the two objects whose publishes actually change slot chrome — and
/// receives live-status flags and tab visibility as values from `ContentView`. It does
/// **not** observe `ChatViewModel`: that type has ~100 `@Published`
/// properties, and a grid that subscribed to it re-laid-out all 40
/// slots on every unrelated write (transcript chunk, hover, panel
/// width). Closures stay outside `==` so a parent rebuild of the
/// toggle handler cannot defeat `.equatable()` at the call site.
///
/// The four pool grids stay permanently in the `ZStack` so switching
/// pools is a display filter — no `dismantleNSView`, no libghostty
/// surface teardown. Hidden grids skip body evaluation via
/// `WorkerGrid.equatable()` when their snapshots have not moved.
struct WorkersDetailView: View, @MainActor Equatable {
    @ObservedObject var workspace: WorkersWorkspaceModel
    @ObservedObject var liveStates: LiveWorkerStateStore
    let isVisible: Bool
    let liveStatusDisabledSlotIDs: Set<Int>
    let onToggleLiveStatus: (Int, Bool) -> Void

    @State private var selectedPool: AgentPoolKind = .bridgeCrew
    @State private var snapshotCache: WorkerSlotSnapshotCache

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
        _snapshotCache = State(initialValue: WorkerSlotSnapshotCache(
            workspace: workspace,
            liveStates: liveStates,
            liveStatusDisabledSlotIDs: liveStatusDisabledSlotIDs
        ))
    }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.workspace === rhs.workspace
            && lhs.liveStates === rhs.liveStates
            && lhs.isVisible == rhs.isVisible
            && lhs.liveStatusDisabledSlotIDs == rhs.liveStatusDisabledSlotIDs
    }

    var body: some View {
        VStack(spacing: 0) {
            poolPickerHeader
            Divider()
            // All grids stay permanently in the view hierarchy so that
            // switching pools is a pure display filter — no SwiftUI identity
            // churn, no dismantleNSView, no libghostty surface teardown.
            // Only the visible grid receives hit-testing and is rendered.
            // Do not replace this ZStack with `if selectedPool ==` (or a
            // lazy stack): `GhosttyTerminalView.dismantleNSView` frees the
            // surface, and remounting restarts the worker's claude session.
            ZStack {
                // Bridge Crew and Lower Decks are the two pages of the same
                // interactive pool, filtered out of the flat `slots` array.
                grid(for: workspace.bridgeCrewSlots, pool: .bridgeCrew)
                    .opacity(selectedPool == .bridgeCrew ? 1 : 0)
                    .allowsHitTesting(selectedPool == .bridgeCrew)

                grid(for: workspace.lowerDecksSlots, pool: .lowerDecks)
                    .opacity(selectedPool == .lowerDecks ? 1 : 0)
                    .allowsHitTesting(selectedPool == .lowerDecks)

                grid(for: workspace.automationSlots, pool: .automations, columns: 3)
                    .opacity(selectedPool == .automations ? 1 : 0)
                    .allowsHitTesting(selectedPool == .automations)

                grid(for: workspace.reviewSlots, pool: .reviewers)
                    .opacity(selectedPool == .reviewers ? 1 : 0)
                    .allowsHitTesting(selectedPool == .reviewers)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .background(Color(nsColor: .separatorColor))
    }

    private func grid(for slots: [WorkerSlot], pool: AgentPoolKind, columns: Int = 4) -> some View {
        WorkerGrid(
            runtime: workspace.runtime,
            snapshots: snapshotCache.snapshots(
                for: pool,
                slots: slots,
                liveStates: liveStates,
                liveStatusDisabledSlotIDs: liveStatusDisabledSlotIDs,
                refresh: isVisible
            ),
            onToggleLiveStatus: onToggleLiveStatus,
            columns: columns
        )
        .equatable()
    }

    private var poolPickerHeader: some View {
        HStack {
            NativeSegmentedPicker(
                "Pool",
                selection: $selectedPool,
                options: AgentPoolKind.allCases,
                title: { pool in
                    pool.label(
                        bridgeCrewCount: workspace.bridgeCrewSlots.count,
                        lowerDecksCount: workspace.lowerDecksSlots.count,
                        automationCount: WorkersWorkspaceModel.automationSlotCount,
                        reviewCount: workspace.reviewSlotCount
                    )
                }
            )
            .frame(maxWidth: 460)
            Spacer()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(Color(nsColor: .windowBackgroundColor))
    }
}

/// Keeps the last visible inputs for each permanently-mounted worker grid.
/// `LiveWorkerStateStore` still invalidates the enclosing view while Agents
/// is hidden, but those invalidations reuse these values. That avoids both
/// snapshot construction and a changed `WorkerGrid` input, while retaining
/// the existing terminal-view hierarchy and its libghostty surfaces.
@MainActor
final class WorkerSlotSnapshotCache {
    private var snapshotsByPool: [AgentPoolKind: [WorkerSlotSnapshot]]

    init(
        workspace: WorkersWorkspaceModel,
        liveStates: LiveWorkerStateStore,
        liveStatusDisabledSlotIDs: Set<Int>
    ) {
        snapshotsByPool = [:]
        refresh(
            .bridgeCrew,
            slots: workspace.bridgeCrewSlots,
            liveStates: liveStates,
            liveStatusDisabledSlotIDs: liveStatusDisabledSlotIDs
        )
        refresh(
            .lowerDecks,
            slots: workspace.lowerDecksSlots,
            liveStates: liveStates,
            liveStatusDisabledSlotIDs: liveStatusDisabledSlotIDs
        )
        refresh(
            .automations,
            slots: workspace.automationSlots,
            liveStates: liveStates,
            liveStatusDisabledSlotIDs: liveStatusDisabledSlotIDs
        )
        refresh(
            .reviewers,
            slots: workspace.reviewSlots,
            liveStates: liveStates,
            liveStatusDisabledSlotIDs: liveStatusDisabledSlotIDs
        )
    }

    func snapshots(
        for pool: AgentPoolKind,
        slots: [WorkerSlot],
        liveStates: LiveWorkerStateStore,
        liveStatusDisabledSlotIDs: Set<Int>,
        refresh shouldRefresh: Bool
    ) -> [WorkerSlotSnapshot] {
        if shouldRefresh {
            refresh(pool, slots: slots, liveStates: liveStates, liveStatusDisabledSlotIDs: liveStatusDisabledSlotIDs)
        }
        return snapshotsByPool[pool] ?? []
    }

    private func refresh(
        _ pool: AgentPoolKind,
        slots: [WorkerSlot],
        liveStates: LiveWorkerStateStore,
        liveStatusDisabledSlotIDs: Set<Int>
    ) {
        snapshotsByPool[pool] = slots.map { slot in
            WorkerSlotSnapshot.build(
                slot: slot,
                liveState: liveStates.bySlot[slot.slotId],
                liveStatusEnabled: !liveStatusDisabledSlotIDs.contains(slot.slotId)
            )
        }
    }
}

/// Eager 4-column (or 3-column automations) grid. Not lazy: every slot
/// of a pool is on-screen in this full-bleed layout, and a `LazyVStack`
/// would let SwiftUI dismantle off-screen `NSViewRepresentable`s —
/// `GhosttyTerminalView.dismantleNSView` frees the libghostty surface.
/// Equatable over the snapshot array so a live-state tick in another
/// pool skips this grid's body.
private struct WorkerGrid: View, @MainActor Equatable {
    let runtime: GhosttyRuntime
    let snapshots: [WorkerSlotSnapshot]
    let onToggleLiveStatus: (Int, Bool) -> Void
    var columns: Int = 4

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.runtime === rhs.runtime
            && lhs.snapshots == rhs.snapshots
            && lhs.columns == rhs.columns
    }

    var body: some View {
        let rows = stride(from: 0, to: snapshots.count, by: columns).map { start in
            Array(snapshots[start..<min(start + columns, snapshots.count)])
        }

        VStack(spacing: 1) {
            ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                HStack(spacing: 1) {
                    ForEach(row, id: \.slotId) { snapshot in
                        WorkerSlotView(
                            runtime: runtime,
                            snapshot: snapshot,
                            onToggleLiveStatus: { enabled in
                                onToggleLiveStatus(snapshot.slotId, enabled)
                            }
                        )
                        .equatable()
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                }
            }
        }
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
            Divider()
            slotBody
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    @ViewBuilder
    private var slotBody: some View {
        if let session = snapshot.session {
            WorkerPaneTerminalView(
                runtime: runtime,
                session: session,
                liveStatePresent: snapshot.live != nil
            )
            // Pin view identity to the session, not just to this `if`
            // branch's position in the tree. A slot can be released and
            // respawned into a NEW session within the same SwiftUI
            // update pass (no intervening render of the `idlePaneView`
            // branch to force teardown) — without an explicit `.id()`,
            // SwiftUI would treat the two sessions as "the same view"
            // and reuse the OLD `GhosttyTerminalHostView`/libghostty
            // surface untouched (`updateNSView` never rebinds it), so
            // the new execution's pane would render against the
            // PREVIOUS tenant's scrollback under the new header. Keying
            // on `session.id` (`"run-<runId>"`, unique per execution)
            // forces `dismantleNSView`/`makeNSView` on every rebind, so
            // the old surface is always torn down and a fresh one
            // created before the new run's output ever lands.
            .id(session.id)
        } else {
            idlePaneView
        }
    }

    /// Idle / free slot treatment: large character portrait + crew name
    /// + a stable in-character recreational flavor line. The line is
    /// keyed on `snapshot.idleFlavorCycle`, which the workspace model
    /// bumps when the slot re-enters idle, so within one idle bout
    /// the line never flickers.
    @ViewBuilder
    private var idlePaneView: some View {
        let character = TrekCharacter.forSlot(snapshot.slotId)
        VStack(spacing: 14) {
            Spacer()
            if let character {
                if let nsImage = TrekIconAssets.image(character, size: .large) {
                    Image(nsImage: nsImage)
                        .resizable()
                        .interpolation(.high)
                        .aspectRatio(contentMode: .fit)
                        .frame(maxWidth: 220, maxHeight: 240)
                        .opacity(0.85)
                }
                Text(character.displayName)
                    .font(.title3.weight(.semibold))
                    .foregroundStyle(Color.white.opacity(0.85))
                Text(TrekIdleFlavor.line(for: character, cycle: snapshot.idleFlavorCycle))
                    .font(.callout)
                    .foregroundStyle(Color.white.opacity(0.6))
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 24)
                    .lineLimit(3)
            } else if WorkersWorkspaceModel.lowerDecksSlotRange.contains(snapshot.slotId) {
                // Lower Decks has no bespoke portrait asset, but it is still a
                // real crew: show the canonical name (same `WorkerNames` source
                // as the running-pane title) so the page reads as a roster
                // rather than bare slot numbers.
                Text(WorkerNames.name(forSlot: snapshot.slotId))
                    .font(.title3.weight(.semibold))
                    .foregroundStyle(Color.white.opacity(0.85))
                Text("Free")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(Color.white.opacity(0.7))
            } else {
                Text("Slot \(snapshot.slotId)")
                    .font(.caption2)
                    .foregroundStyle(Color.white.opacity(0.45))
                Text("Free")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(Color.white.opacity(0.7))
            }
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .black))
    }

    private var slotHeader: some View {
        HStack(spacing: 8) {
            if let character = TrekCharacter.forSlot(snapshot.slotId),
               let nsImage = TrekIconAssets.image(character, size: .small) {
                Image(nsImage: nsImage)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fit)
                    .frame(width: 22, height: 28)
                    .clipShape(RoundedRectangle(cornerRadius: 3, style: .continuous))
            }

            VStack(alignment: .leading, spacing: 1) {
                slotTaskLine

                slotSubtitle
            }

            Spacer(minLength: 0)

            // Prefer engine-supplied LiveWorkerState — its activity is
            // driven by hook events rather than a screen-scrape that
            // always rendered "Agent Unknown". Fall back to a session-
            // observing pill until the worker's first hook fires, so
            // `.equatable()` skipping this body does not freeze the
            // fallback chrome.
            if let live = snapshot.live {
                statusPill(
                    live.activity.label,
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
        let base = "Worker \(snapshot.slotId)"
        if let runId = snapshot.runId {
            return "\(base) · run \(runId)"
        }
        return "\(base) · idle"
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
        let name = WorkerNames.name(forSlot: snapshot.slotId)
        let text: String = {
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
