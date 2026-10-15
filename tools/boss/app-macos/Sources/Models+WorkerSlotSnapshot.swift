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

    init(_ live: WorkerLiveState) {
        self.name = live.name
        self.activity = live.activity
        self.liveStatus = live.liveStatus
        self.recoveryStatus = live.recoveryStatus
        self.lastEventAt = live.lastEventAt
        self.agentType = live.agentType
        self.projectName = live.projectName
        self.pool = live.pool
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

    var displayName: String {
        if let cachedPersona, !cachedPersona.isEmpty { return cachedPersona }
        let runId = runId ?? ""
        return WorkerLiveState.displayName(name: live?.name, runId: runId)
    }

    var badgeType: AgentType { live?.badgeType ?? .unknown }
    var isExited: Bool { live?.isExited == true }
    var isViewerMissing: Bool { runId != nil && session == nil && !isPlaceholder }

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
    }

    static func build(
        slot: WorkerSlot,
        liveState: WorkerLiveState?,
        liveStatusEnabled: Bool,
        isPlaceholder: Bool = false,
        cachedPersona: String? = nil
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
            cachedPersona: cachedPersona
        )
    }
}
