import Foundation

struct EngineSpawnRequest: Sendable {
    let runId: String
    let workspacePath: String
    /// 1-indexed slot the engine has claimed for this worker. The
    /// app must host the pane in this exact slot or fail with
    /// `.slotBusy`. The engine is the source of truth for slot
    /// allocation; the previous `firstIndex(where:)` heuristic in
    /// the app has been removed.
    let slotId: Int
    let initialInput: String
    let env: [(String, String)]
    /// Engine-supplied 2–4 word present-continuous gerund phrase
    /// describing what the worker is doing (e.g. "fixing the fencer
    /// scraper"). Present only when the engine successfully called
    /// Claude to generate the phrase. When nil, use `taskTitle` for
    /// the fallback format `"<AgentName>: <taskTitle>"`.
    let summary: String?
    /// Raw work-item title (the task's name column). Used as the
    /// fallback display label when `summary` is nil — rendered as
    /// `"<AgentName>: <taskTitle>"` rather than with a gerund "is".
    let taskTitle: String?
}

enum EngineSpawnError: Sendable {
    case noAvailableSlot
    /// Engine asked us to host the pane in a slot that already has a
    /// session. Surfaces engine↔app disagreement explicitly instead
    /// of silently re-allocating to a different slot, which would
    /// re-introduce the dual-allocator bug the engine-owns-slots
    /// refactor exists to fix. `occupyingRunId` is whatever run the
    /// slot is currently hosting, so the engine can log which pane
    /// caused the rejection instead of just "SlotBusy".
    case slotBusy(occupyingRunId: String?)
    case internalFailure(String)
}

enum EngineSpawnResult: Sendable {
    case success(slotId: Int, shellPid: Int32)
    case failure(EngineSpawnError)
}

enum EngineReleaseError: Sendable {
    case unknownSlot
    case internalFailure(String)
}

enum EngineReleaseResult: Sendable {
    case success
    case failure(EngineReleaseError)
}

enum EngineSendError: Sendable {
    case unknownSlot
    case internalFailure(String)
}

enum EngineSendResult: Sendable {
    case success
    case failure(EngineSendError)
}

enum EngineFocusError: Sendable {
    case unknownSlot
    case internalFailure(String)
}

enum EngineFocusResult: Sendable {
    case success
    case failure(EngineFocusError)
}

enum EngineInterruptError: Sendable {
    case unknownSlot
    case internalFailure(String)
}

enum EngineInterruptResult: Sendable {
    case success
    case failure(EngineInterruptError)
}

enum EngineRevealError: Sendable {
    case internalFailure(String)
}

enum EngineRevealResult: Sendable {
    case success
    case failure(EngineRevealError)
}

/// One slot the app reports as currently hosting a session, in reply
/// to `EngineRequestKind.listHostedPanes`. Mirrors the wire
/// `HostedPaneEntry` shape.
struct EngineHostedPaneEntry: Sendable {
    let slotId: Int
    let runId: String
    let summary: String?
    let taskTitle: String?
}

enum EngineRequestKind: Sendable {
    case spawnWorkerPane(EngineSpawnRequest)
    case releaseWorkerPane(slotId: Int, killGraceSeconds: UInt32)
    case sendToPane(slotId: Int, text: String)
    case focusWorkerPane(slotId: Int)
    case interruptWorkerPane(slotId: Int)
    case revealWorkItem(workItemId: String, productId: String)
    /// Read-only: engine asks which slots the app currently hosts a
    /// session in, regardless of whether the engine has a live-tracked
    /// run for them. Backs `bossctl agents list --all` — the engine
    /// diffs the reply against its own live-worker registry to surface
    /// "husk" panes.
    case listHostedPanes
}
