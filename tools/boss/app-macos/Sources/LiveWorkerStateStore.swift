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

    /// Count of workers in a non-terminal "alive" state: `spawning`,
    /// `working`, or `waitingForInput`. Used by the quit-confirmation
    /// guard — a worker idle at a prompt still has live conversation
    /// history and possibly in-progress edits in its leased workspace,
    /// so the operator should see the confirmation either way. What
    /// quitting then *does* depends on hosting mode (see
    /// [[QuitConfirmation]]). Excludes `idle`, `errored`, and
    /// `terminated`.
    var activeAgentCount: Int {
        let live: Set<WorkerActivity> = [.spawning, .working, .waitingForInput]
        return bySlot.values.filter { live.contains($0.activity) }.count
    }

    /// Replace the snapshot with `states`. Skips the publish when the
    /// new snapshot is value-equal to the previous one — a hook event
    /// that nudged `lastEventAt` but left every per-slot field
    /// otherwise unchanged still reaches us, and republishing it would
    /// invalidate every observing card for no visible delta.
    func update(states: [WorkerLiveState]) {
        let newByRunID = Dictionary(uniqueKeysWithValues: states.map { ($0.runId, $0) })
        let newBySlot = Dictionary(uniqueKeysWithValues: states.map { ($0.slotId, $0) })
        if newByRunID != byRunID {
            byRunID = newByRunID
        }
        if newBySlot != bySlot {
            bySlot = newBySlot
        }
    }
}
