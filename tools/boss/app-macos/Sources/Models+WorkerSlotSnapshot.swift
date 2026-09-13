import Foundation

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
    let activity: WorkerActivity
    let liveStatus: String?
    let recoveryStatus: String?
    let lastEventAt: String?

    init(_ live: WorkerLiveState) {
        self.activity = live.activity
        self.liveStatus = live.liveStatus
        self.recoveryStatus = live.recoveryStatus
        self.lastEventAt = live.lastEventAt
    }
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
    let idleFlavorCycle: Int
    /// Compared by identity in `==`. `nil` means the idle portrait
    /// branch; non-nil means `WorkerPaneTerminalView` stays mounted.
    let session: TerminalPaneSession?
    let liveStatusEnabled: Bool
    let live: WorkerSlotLiveSlice?

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.slotId == rhs.slotId
            && lhs.runId == rhs.runId
            && lhs.summary == rhs.summary
            && lhs.taskTitle == rhs.taskTitle
            && lhs.idleFlavorCycle == rhs.idleFlavorCycle
            && lhs.session === rhs.session
            && lhs.liveStatusEnabled == rhs.liveStatusEnabled
            && lhs.live == rhs.live
    }

    static func build(
        slot: WorkerSlot,
        liveState: WorkerLiveState?,
        liveStatusEnabled: Bool
    ) -> WorkerSlotSnapshot {
        WorkerSlotSnapshot(
            slotId: slot.slotId,
            runId: slot.runId,
            summary: slot.summary,
            taskTitle: slot.taskTitle,
            idleFlavorCycle: slot.idleFlavorCycle,
            session: slot.session,
            liveStatusEnabled: liveStatusEnabled,
            live: liveState.map(WorkerSlotLiveSlice.init)
        )
    }
}
