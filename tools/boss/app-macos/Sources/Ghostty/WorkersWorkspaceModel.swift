import AppKit
import Darwin
import Foundation
import GhosttyKit

@MainActor
final class WorkersWorkspaceModel: ObservableObject {
    /// Interactive/main pool capacity. Mirrors `MAX_WORKER_POOL_SIZE`.
    /// Slot ids 1...16 remain engine capacity handles; they are not
    /// Agents-view pages.
    static let workerSlotCount = 16
    /// Automation pool occupies slot IDs immediately above the interactive pool.
    /// Matches MAX_AUTOMATION_POOL_SIZE in coordinator.rs.
    static let automationSlotCount = 8
    static let automationSlotBase = workerSlotCount + 1   // 17
    static let automationSlotRange = automationSlotBase...(automationSlotBase + automationSlotCount - 1)  // 17...24
    /// Review pool occupies slot IDs immediately above the automation pool.
    /// The count is set dynamically via configureSlots(workerCount:automationCount:reviewCount:)
    /// when the engine pushes EnginePoolConfig on RegisterAppSession.
    /// The initial value of 16 matches DEFAULT_REVIEW_POOL_SIZE in coordinator.rs.
    static let reviewSlotBase = automationSlotBase + automationSlotCount   // 25

    /// Engine-pushed pool sizes for the Agents pool strip. Existing
    /// viewers survive capacity changes until detached.
    @Published private(set) var interactivePoolSize: Int = 16
    @Published private(set) var automationPoolSize: Int = 8
    @Published private(set) var reviewSlotCount: Int = 16

    var reviewSlotRange: ClosedRange<Int> {
        WorkersWorkspaceModel.reviewSlotBase...(WorkersWorkspaceModel.reviewSlotBase + reviewSlotCount - 1)
    }

    let runtime: GhosttyRuntime
    /// Authoritative viewer identity, keyed by run id. Layout cells are
    /// a separate presentation concern in `AgentsGridModel`.
    @Published private(set) var viewersByRunId: [String: WorkerSlot] = [:]

    init() {
        self.runtime = GhosttyRuntime.shared
    }

    /// Update pool capacities from the engine's EnginePoolConfig push.
    /// Called every time the app registers a session, so the slot ranges
    /// always mirror the live engine rather than independently-maintained
    /// constants. Existing viewers survive capacity changes until detached.
    func configureSlots(workerCount: Int, automationCount: Int, reviewCount: Int) {
        if interactivePoolSize != workerCount {
            interactivePoolSize = workerCount
        }
        if automationPoolSize != automationCount {
            automationPoolSize = automationCount
        }
        if reviewSlotCount != reviewCount {
            reviewSlotCount = reviewCount
        }
    }

    /// Attach a Ghostty surface to an already-running tmux worker. The app
    /// supplies neither the worker's environment nor its working directory:
    /// those were fixed when the engine created the detached tmux session.
    /// Closing this surface must therefore not report a worker death.
    ///
    /// The engine is the source of truth for slot allocation: `hostAttachedPane`
    /// honors the requested slot or fails — it never picks a different slot.
    func attachWorkerPane(_ request: EngineAttachRequest) -> EngineAttachResult {
        guard !request.tmuxSocketPath.isEmpty, request.tmuxSocketPath.hasPrefix("/") else {
            return .failure(.internalFailure("engine supplied an invalid tmux socket path"))
        }
        let launch = WorkerViewerLaunch(attaching: request)
        switch hostAttachedPane(launch) {
        case .success:
            return .success
        case .failure(let error):
            return .failure(error)
        }
    }

    /// Host an attached worker pane in the slot the engine has claimed for
    /// this worker (`request.slotId`). An existing run keeps its surface and
    /// original slot metadata and succeeds before slot allocation checks.
    ///
    /// Main-pool slots occupy 1...\(workerSlotCount); automation-pool
    /// slots occupy \(automationSlotBase)...\(automationSlotBase + automationSlotCount - 1).
    ///
    /// Returns:
    ///  - `.failure(.internalFailure)` if `slotId` is outside the known
    ///    ranges (engine asked for a slot that doesn't exist on this app).
    ///  - `.failure(.slotBusy)` if the requested slot already hosts
    ///    a session (engine and app disagree about what's free —
    ///    the engine should reconcile rather than retry blindly).
    private func hostAttachedPane(_ request: WorkerViewerLaunch) -> EngineSpawnResult {
        // Reconnection/retry must preserve the exact surface, even if slot
        // metadata changed. Run identity alone selects an existing viewer.
        if let existing = viewersByRunId[request.runId] {
            return .success(slotId: existing.slotId)
        }
        let slotId = request.slotId
        guard (1...Self.workerSlotCount).contains(slotId)
                || Self.automationSlotRange.contains(slotId)
                || reviewSlotRange.contains(slotId) else {
            return .failure(.internalFailure("engine requested unknown slot \(slotId)"))
        }
        if let occupant = viewersByRunId.values.first(where: { $0.slotId == slotId }) {
            return .failure(.slotBusy(occupyingRunId: occupant.runId))
        }

        let launchSpec = TerminalLaunchSpec(
            fontSize: 10.0,
            workingDirectory: FileManager.default.homeDirectoryForCurrentUser.path,
            initialInput: request.initialInput,
            env: []
        )
        let session = TerminalPaneSession(
            id: "run-\(request.runId)",
            role: .worker(slot: slotId),
            launchSpec: launchSpec
        )
        viewersByRunId[request.runId] = WorkerSlot(
            slotId: slotId,
            session: session,
            runId: request.runId,
            summary: request.summary,
            taskTitle: request.taskTitle
        )

        SpawnDiagnosticsLog.shared.spawnRequested(
            runId: request.runId,
            slotId: slotId,
            sessionName: request.sessionName,
            tmuxSocketPath: request.tmuxSocketPath
        )
        return .success(slotId: slotId)
    }

    /// Detach a tmux viewer surface. The worker process is owned by the
    /// detached tmux session, not by Ghostty, so this does not inspect or
    /// signal any foreground process — it only tears down the app's own
    /// viewer bookkeeping.
    func detachWorkerPane(runId: String) -> EngineReleaseResult {
        guard let viewer = viewersByRunId[runId], let session = viewer.session else {
            return .failure(.unknownSlot)
        }
        // Prevent a display-change retry from recreating a released surface.
        session.markReleased()
        viewersByRunId.removeValue(forKey: runId)
        return .success
    }

    /// Report every slot currently hosting a session, across all three
    /// pools, regardless of whether the engine has a live-tracked run
    /// for it. Answers `EngineRequestKind.listHostedPanes` — the
    /// engine diffs this against its own live-worker registry to
    /// surface "husk" panes for `bossctl agents list --all`.
    func listHostedPanes() -> [EngineHostedPaneEntry] {
        viewersByRunId.values.sorted { $0.slotId < $1.slotId }.compactMap { slot in
            guard slot.session != nil, let runId = slot.runId else { return nil }
            return EngineHostedPaneEntry(
                slotId: slot.slotId,
                runId: runId,
                summary: slot.summary,
                taskTitle: slot.taskTitle
            )
        }
    }

    /// Bring the slot's libghostty surface to first responder and
    /// raise the host window. Mirrors the user-click path in
    /// `GhosttyTerminalHostView.mouseDown` (which also calls
    /// `makeFirstResponder(self)`), then activates the application so
    /// the window is visible if it was minimised or behind another
    /// app. Used by `bossctl agents focus`.
    func focusWorkerPane(runId: String) -> EngineFocusResult {
        guard let session = viewersByRunId[runId]?.session else {
            return .failure(.unknownSlot)
        }
        guard let host = session.hostView else {
            return .failure(.internalFailure("pane has no live surface"))
        }
        guard let window = host.window else {
            // No host window means the pane isn't on screen yet
            // (NSView never moved into a window). The slot is
            // allocated but unrenderable, so refuse instead of
            // silently no-op'ing.
            return .failure(.internalFailure("pane has no host window"))
        }
        NSApp.activate(ignoringOtherApps: true)
        if window.isMiniaturized {
            window.deminiaturize(nil)
        }
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(host)
        return .success
    }
}

struct WorkerSlot: Identifiable, Equatable {
    let slotId: Int
    var session: TerminalPaneSession?
    var runId: String?
    /// Short present-continuous gerund phrase the engine generated for
    /// this run via Claude (e.g. `"fixing the fencer scraper"`).
    /// Rendered in the pane titlebar as `"<WorkerName> is <phrase>"`.
    /// Present only when ANTHROPIC_API_KEY was available and the
    /// Claude call succeeded. When nil, `taskTitle` is used instead.
    var summary: String?
    /// Raw work-item title (the task's name column). Used when
    /// `summary` is nil — rendered as `"<WorkerName>: <taskTitle>"`
    /// so the header still identifies the task without a gerund.
    var taskTitle: String?

    var id: Int { slotId }

    static func == (lhs: WorkerSlot, rhs: WorkerSlot) -> Bool {
        lhs.slotId == rhs.slotId
            && lhs.runId == rhs.runId
            && lhs.summary == rhs.summary
            && lhs.taskTitle == rhs.taskTitle
            && lhs.session === rhs.session
    }
}

extension WorkerViewerLaunch {
    /// Viewer-launch parameters derived from `AttachWorkerPane`.
    ///
    /// The tmux client's working directory (the current user's home) is
    /// applied in `hostAttachedPane`, not carried here — the attach RPC
    /// does not include a worker workspace path. Spawn diagnostics must
    /// record `sessionName` and `tmuxSocketPath` instead of treating that
    /// directory as `workspace_path`.
    init(attaching request: EngineAttachRequest) {
        self.init(
            runId: request.runId,
            slotId: request.slotId,
            initialInput: "exec tmux -S \(bossShellQuote(request.tmuxSocketPath)) attach-session -t \(bossShellQuote(request.sessionName))\n",
            summary: request.summary,
            taskTitle: request.taskTitle,
            sessionName: request.sessionName,
            tmuxSocketPath: request.tmuxSocketPath
        )
    }
}
