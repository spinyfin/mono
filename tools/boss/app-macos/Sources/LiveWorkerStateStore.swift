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

    /// Workers in a non-terminal "alive" state: `spawning`, `working`,
    /// or `waitingForInput`. Shared by `activeAgentCount` and
    /// `activeAgentTmuxHostedFlags` so the quit dialog's count and
    /// classified makeup cannot drift. Excludes `idle`, `errored`, and
    /// `terminated`.
    private static let aliveActivities: Set<WorkerActivity> = [
        .spawning, .working, .waitingForInput,
    ]

    /// Count of currently alive workers. Used by the quit-confirmation
    /// guard — from the user's perspective a worker idle at a Claude
    /// prompt is still kill-worthy (live conversation history,
    /// possibly in-progress edits in its leased workspace).
    var activeAgentCount: Int {
        activeAgentTmuxHostedFlags.count
    }

    /// `tmuxHosted` of every currently active worker (same "alive" filter
    /// as `activeAgentCount`), in no particular order. Feeds the
    /// quit-confirmation dialog's hosting-mode claim — see
    /// `QuitConfirmation.HostingMakeup.classify`. Each entry mirrors the
    /// worker's actual dispatch-time hosting mode, not the current
    /// `workers.tmux_hosting` setting value.
    var activeAgentTmuxHostedFlags: [Bool?] {
        bySlot.values.filter { Self.aliveActivities.contains($0.activity) }.map(\.tmuxHosted)
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
