import Foundation
import PaneLayout

// ===========================================================================
// Equatable view-input snapshot for one Agents-tab worker slot.
// Mirrors `WorkCardSnapshot`: a small value type holding exactly the
// fields `WorkerSlotView` renders, so `.equatable()` can skip a slot
// body when an unrelated `ChatViewModel` publish (or another slot's
// live-state tick) invalidates the parent.
// ===========================================================================

/// Slim live-state slice that the Agents slot header actually paints.
/// `WorkerLiveState` carries extra fields (`model`, `shellPid`,
/// `currentTool`, `tmuxHosted`, …) the slot view never reads; folding
/// the whole struct into the snapshot would re-evaluate every slot on
/// those unused writes.
struct WorkerSlotLiveSlice: Equatable {
    let name: String?
    let activity: WorkerActivity
    let liveStatus: String?
    let recoveryStatus: String?
    let lastEventAt: String?
    let agentType: String?
    let projectName: String?
    let pool: String?
    /// Read by the viewer-missing card only: `false` means the engine can
    /// never attach a pane (no durable tmux identity), so the card must not
    /// promise "Connecting…".
    let tmuxHosted: Bool?
    /// Engine-reported reason the last viewer attach failed; `nil` while
    /// nothing has failed. Read by the viewer-missing card only.
    let viewerAttachError: String?

    init(_ live: WorkerLiveState) {
        self.name = live.name
        self.activity = live.activity
        self.liveStatus = live.liveStatus
        self.recoveryStatus = live.recoveryStatus
        self.lastEventAt = live.lastEventAt
        self.agentType = live.agentType
        self.projectName = live.projectName
        self.pool = live.pool
        self.tmuxHosted = live.tmuxHosted
        self.viewerAttachError = live.viewerAttachError
    }

    var badgeType: AgentType { AgentType.fromWire(agentType) }
    var isExited: Bool { activity == .terminated }
}

/// Per-slot view input. Closures stay off this type so a parent rebuild
/// of `onToggleLiveStatus` cannot defeat `.equatable()`. `session` is
/// compared by identity: the terminal `NSViewRepresentable` is keyed on
/// the same object, and pointer equality is the cheap "same pane"
/// signal. Equatable is isolated to the main actor (same shape as
/// `WorkBoardCardView`) because `session` is a `@MainActor` class.
struct WorkerSlotSnapshot: @MainActor Equatable {
    let slotId: Int
    let runId: String?
    let summary: String?
    let taskTitle: String?
    /// Compared by identity in `==`. `nil` with a run means the
    /// viewer-missing card; non-nil means `WorkerPaneTerminalView`
    /// stays mounted.
    let session: TerminalPaneSession?
    let liveStatusEnabled: Bool
    let live: WorkerSlotLiveSlice?
    /// Engine-released cell retained by a layout epoch ("Riker finished").
    let isPlaceholder: Bool
    /// Cached persona for placeholders after live state is gone.
    let cachedPersona: String?
    /// Why this app refused the engine's last `AttachWorkerPane` for the
    /// run (slot busy, unknown slot, bad socket path), recorded by
    /// `WorkersWorkspaceModel`. `nil` when the app never rejected one.
    /// The app-side reason wins over the engine's: it is the more specific
    /// half of the same failed exchange.
    let viewerAttachFailure: String?

    var displayName: String {
        if let cachedPersona, !cachedPersona.isEmpty { return cachedPersona }
        let runId = runId ?? ""
        return WorkerLiveState.displayName(name: live?.name, runId: runId)
    }

    var badgeType: AgentType { live?.badgeType ?? .unknown }
    var isExited: Bool { live?.isExited == true }
    /// A finished placeholder is historical text, not an addressable worker.
    var offersLiveStatusToggle: Bool { !isPlaceholder }
    var isViewerMissing: Bool { runId != nil && session == nil && !isPlaceholder }

    /// Caption for the viewer-missing card. A live member legitimately has
    /// no viewer only while the engine's `AttachWorkerPane` is still on its
    /// way (spawn, app or engine restart re-attach, the app's own
    /// re-attach request); that reads "Connecting…". Anything the app or
    /// the engine knows went wrong is shown instead, most specific first,
    /// so a failed attach is never dressed up as still connecting.
    var viewerMissingMessage: String {
        if isExited { return "Worker exited" }
        if let viewerAttachFailure, !viewerAttachFailure.isEmpty {
            return "Couldn't attach the terminal: \(viewerAttachFailure)"
        }
        if let error = live?.viewerAttachError, !error.isEmpty {
            return "Couldn't attach the terminal: \(error)"
        }
        if live?.tmuxHosted == false {
            return AgentActivityState.identityUnavailableReason
        }
        return "Connecting…"
    }

    /// True while the card's only honest caption is "Connecting…" — no
    /// failure is known on either side. The view asks the engine for the
    /// viewer in this state when the pane comes into view.
    var isViewerConnecting: Bool {
        isViewerMissing && !isExited && viewerAttachFailure == nil && live?.viewerAttachError == nil && live?.tmuxHosted != false
    }

    /// What the slot body draws. A released run is always the finished card,
    /// whether or not its viewer is still attached; an attached surface stays
    /// mounted (hidden) until the matching detach. The viewer-missing card is
    /// reserved for a run the engine still lists.
    enum BodyKind: Equatable {
        case placeholder
        case terminal
        case viewerMissing
    }

    var bodyKind: BodyKind {
        if isPlaceholder { return .placeholder }
        return session != nil ? .terminal : .viewerMissing
    }

    /// First titlebar line. A finished run never reads as working: the
    /// placeholder text wins over any summary or task title.
    var headerTitle: String {
        let name = displayName
        if isPlaceholder {
            return "\(name) finished"
        }
        if let summary, !summary.isEmpty {
            // Success path: Claude-generated gerund phrase.
            // Preserved verbatim — do NOT change this branch.
            return "\(name) is \(summary)"
        }
        if let taskTitle, !taskTitle.isEmpty {
            // Fallback path: no gerund available (no API key or
            // summarization failed). Use "<Name>: <task>" format.
            return "\(name): \(taskTitle)"
        }
        if runId != nil {
            return "\(name) is working"
        }
        return name
    }

    /// Type badge in the titlebar and on the viewer-missing card. Only an
    /// engine-listed run has a type to show; a finished run shows none.
    var headerBadge: AgentType? {
        guard !isPlaceholder, live != nil else { return nil }
        return badgeType
    }

    /// Titlebar status pill text. `nil` means no engine state yet: the view
    /// falls back to the session-observing pane-monitor pill.
    var statusPillLabel: String? {
        if isPlaceholder { return "Finished" }
        return live?.activity.agentsPaneLabel
    }

    /// Whether the live-status toggle is addressable. A finished placeholder
    /// is historical text, not a worker: its slot may already belong to a
    /// new run.
    var showsLiveStatusToggle: Bool { !isPlaceholder }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.slotId == rhs.slotId
            && lhs.runId == rhs.runId
            && lhs.summary == rhs.summary
            && lhs.taskTitle == rhs.taskTitle
            && lhs.session === rhs.session
            && lhs.liveStatusEnabled == rhs.liveStatusEnabled
            && lhs.live == rhs.live
            && lhs.isPlaceholder == rhs.isPlaceholder
            && lhs.cachedPersona == rhs.cachedPersona
            && lhs.viewerAttachFailure == rhs.viewerAttachFailure
    }

    static func build(
        slot: WorkerSlot,
        liveState: WorkerLiveState?,
        liveStatusEnabled: Bool,
        isPlaceholder: Bool = false,
        cachedPersona: String? = nil,
        viewerAttachFailure: String? = nil
    ) -> WorkerSlotSnapshot {
        WorkerSlotSnapshot(
            slotId: slot.slotId,
            runId: slot.runId,
            summary: slot.summary,
            taskTitle: slot.taskTitle,
            session: slot.session,
            liveStatusEnabled: liveStatusEnabled,
            live: liveState.map(WorkerSlotLiveSlice.init),
            isPlaceholder: isPlaceholder,
            cachedPersona: cachedPersona,
            viewerAttachFailure: viewerAttachFailure
        )
    }

    /// The Agents grid's render rule for one mounted surface, derived from
    /// the current inputs only — never from an earlier render. A run the
    /// engine lists is live; a run it no longer lists is finished, whatever
    /// the layout, the viewer map, or tab visibility were doing when the
    /// release arrived. There is no third state: a released run cannot be
    /// drawn as a viewerless live member with an "Unknown" type.
    ///
    /// A viewer attached before its first live-state snapshot reads as
    /// finished here, but it is never placed in the layout (membership comes
    /// from live state only), so it is mounted hidden and never drawn.
    ///
    /// `viewer` is the run's attached pane, if any; without one the slot
    /// metadata comes from the live state (the engine's slot id) so a
    /// release followed by immediate slot reuse still attributes the
    /// finished card to the right run.
    static func render(
        runId: String,
        viewer: WorkerSlot?,
        liveState: WorkerLiveState?,
        liveStatusDisabledSlotIDs: Set<Int>,
        cachedPersona: String?,
        viewerAttachFailure: String? = nil
    ) -> WorkerSlotSnapshot {
        let slotId = viewer?.slotId ?? liveState?.slotId ?? 0
        let slot = viewer ?? WorkerSlot(slotId: slotId, runId: runId, summary: nil, taskTitle: nil)
        return build(
            slot: slot,
            liveState: liveState,
            liveStatusEnabled: !liveStatusDisabledSlotIDs.contains(slotId),
            isPlaceholder: liveState == nil,
            cachedPersona: cachedPersona,
            viewerAttachFailure: viewerAttachFailure
        )
    }
}
