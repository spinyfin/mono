import Foundation

/// Holds the engine's `worker.live_states` snapshot in its own
/// observable, so a hook event arriving on every Claude tool turn does
/// not invalidate every view that observes `ChatViewModel`. Only the
/// kanban Doing-column cards and the worker pane titlebar consume
/// this; everything else (toolbar pickers, sidebar, Boss panel)
/// observes only `ChatViewModel` and is unaffected by the high-rate
/// `worker.live_states` traffic.
@MainActor
final class LiveWorkerStateStore: ObservableObject {
    @Published private(set) var byRunID: [String: WorkerLiveState] = [:]
    @Published private(set) var bySlot: [Int: WorkerLiveState] = [:]

    /// Workers in a non-terminal "alive" state. Only `errored` and
    /// `terminated` are excluded; an `idle` worker still holds its slot
    /// and conversation state.
    private static let aliveActivities: Set<WorkerActivity> = [
        .idle, .spawning, .working, .waitingForInput,
    ]

    /// Count of currently alive workers. Used by the quit-confirmation
    /// guard — from the user's perspective a worker idle at a Claude
    /// prompt is still worth confirming (live conversation history,
    /// possibly in-progress edits in its leased workspace).
    @Published private(set) var activeAgentCount = 0

    /// Replace the snapshot with `states`. Skips the publish when the
    /// new snapshot is value-equal to the previous one — a hook event
    /// that nudged `lastEventAt` but left every per-slot field
    /// otherwise unchanged still reaches us, and republishing it would
    /// invalidate every observing card for no visible delta.
    func update(states: [WorkerLiveState]) {
        guard WorkerLiveState.hasUniqueIds(states) else { return }
        let newByRunID = Dictionary(uniqueKeysWithValues: states.map { ($0.runId, $0) })
        let newBySlot = Dictionary(uniqueKeysWithValues: states.map { ($0.slotId, $0) })
        let newActiveAgentCount = newBySlot.values.count { Self.aliveActivities.contains($0.activity) }
        if newByRunID != byRunID {
            byRunID = newByRunID
        }
        if newBySlot != bySlot {
            bySlot = newBySlot
        }
        if newActiveAgentCount != activeAgentCount {
            activeAgentCount = newActiveAgentCount
        }
    }
}
