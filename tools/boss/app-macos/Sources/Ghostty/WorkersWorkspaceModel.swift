import AppKit
import Darwin
import Foundation
import GhosttyKit

@MainActor
final class WorkersWorkspaceModel: ObservableObject {
    /// Interactive worker slots shown on one page (one "Pool" tab). Mirrors
    /// WORKER_PAGE_SIZE in coordinator.rs.
    static let workerPageSize = 8
    /// Number of interactive pages: page 0 "Bridge Crew", page 1 "Lower Decks".
    /// Mirrors WORKER_PAGE_COUNT in coordinator.rs.
    static let workerPageCount = 2
    /// Total interactive/main pool capacity = pages × page size (currently 16).
    /// Mirrors MAX_WORKER_POOL_SIZE in coordinator.rs; every derived base below
    /// keys off it so engine and app agree on the slot namespace across pages.
    static let workerSlotCount = workerPageSize * workerPageCount
    /// Bridge Crew occupies the first page of slot IDs (1...8); Lower Decks the
    /// second (9...16). Dispatch fills Bridge Crew before spilling into Lower
    /// Decks, but both are the same engine pool — indistinguishable except for
    /// claim-time scheduling priority.
    static let bridgeCrewSlotRange = 1...workerPageSize                              // 1...8
    static let lowerDecksSlotRange = (workerPageSize + 1)...workerSlotCount          // 9...16
    /// Automation pool occupies slot IDs immediately above the interactive pool.
    /// Matches MAX_AUTOMATION_POOL_SIZE in coordinator.rs.
    static let automationSlotCount = 8
    static let automationSlotBase = workerSlotCount + 1   // 17
    static let automationSlotRange = automationSlotBase...(automationSlotBase + automationSlotCount - 1)  // 17...24
    /// Review pool occupies slot IDs immediately above the automation pool.
    /// The count is set dynamically via configureSlots(workerCount:automationCount:reviewCount:)
    /// when the engine pushes EnginePoolConfig on RegisterAppSession, so the
    /// app never independently hardcodes a value that drifts from the engine.
    /// The initial value of 16 matches DEFAULT_REVIEW_POOL_SIZE in coordinator.rs
    /// and ensures the slot grid renders correctly before the first pool-config
    /// push arrives (covering the unlikely race of an AttachWorkerPane before
    /// EnginePoolConfig, and preventing an empty grid on first launch).
    static let reviewSlotBase = automationSlotBase + automationSlotCount   // 25

    /// Instance-level review slot count, kept in sync with the engine's live
    /// pool config. Published so the pool-picker header re-renders whenever
    /// the engine reports a pool size change on reconnect.
    @Published private(set) var reviewSlotCount: Int = 16

    var reviewSlotRange: ClosedRange<Int> {
        WorkersWorkspaceModel.reviewSlotBase...(WorkersWorkspaceModel.reviewSlotBase + reviewSlotCount - 1)
    }

    let runtime: GhosttyRuntime
    @Published private(set) var slots: [WorkerSlot]
    /// Automation-pool slots. These are always idle until the engine wires
    /// up automation pane spawning; the pool-switcher UI shows them so the
    /// slot grid is visible before any automation worker runs.
    @Published private(set) var automationSlots: [WorkerSlot]
    /// Review-pool slots. Mirror the automation pool layout; always idle
    /// until the engine routes a `pr_review` execution to this pool.
    @Published private(set) var reviewSlots: [WorkerSlot]

    /// The interactive pool split into its two display pages. Both are drawn
    /// from `slots` (the single main-pool array) so spawn/release routing stays
    /// keyed on the flat slot id — the pages are a pure display grouping.
    var bridgeCrewSlots: [WorkerSlot] {
        slots.filter { Self.bridgeCrewSlotRange.contains($0.slotId) }
    }
    var lowerDecksSlots: [WorkerSlot] {
        slots.filter { Self.lowerDecksSlotRange.contains($0.slotId) }
    }

    init() {
        self.runtime = GhosttyRuntime.shared
        self.slots = (1...Self.workerSlotCount).map { slot in
            WorkerSlot(slotId: slot, idleFlavorCycle: Int.random(in: 0...10_000))
        }
        self.automationSlots = (Self.automationSlotBase...(Self.automationSlotBase + Self.automationSlotCount - 1)).map { slot in
            WorkerSlot(slotId: slot, idleFlavorCycle: Int.random(in: 0...10_000))
        }
        self.reviewSlots = (Self.reviewSlotBase...(Self.reviewSlotBase + 16 - 1)).map { slot in
            WorkerSlot(slotId: slot, idleFlavorCycle: Int.random(in: 0...10_000))
        }
    }

    /// Update pool capacities from the engine's EnginePoolConfig push.
    /// Called every time the app registers a session, so the slot ranges
    /// always mirror the live engine rather than independently-maintained
    /// constants. Rebuilds the reviewer slot array when the count changes.
    func configureSlots(workerCount: Int, automationCount: Int, reviewCount: Int) {
        guard reviewSlotCount != reviewCount else { return }
        reviewSlotCount = reviewCount
        let base = WorkersWorkspaceModel.reviewSlotBase
        reviewSlots = (base...(base + reviewCount - 1)).map { slot in
            WorkerSlot(slotId: slot, idleFlavorCycle: Int.random(in: 0...10_000))
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
        let launch = EngineSpawnRequest(attaching: request)
        switch hostAttachedPane(launch) {
        case .success:
            return .success
        case .failure(let error):
            return .failure(error)
        }
    }

    /// Host an attached worker pane in the slot the engine has claimed for
    /// this worker (`request.slotId`).
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
    private func hostAttachedPane(_ request: EngineSpawnRequest) -> EngineSpawnResult {
        let requestedSlot = request.slotId
        let isAutomation = Self.automationSlotRange.contains(Int(requestedSlot))
        let isReview = reviewSlotRange.contains(Int(requestedSlot))
        let targetSlots: [WorkerSlot] = isReview ? reviewSlots : (isAutomation ? automationSlots : slots)
        guard requestedSlot >= 1,
              (requestedSlot <= Self.workerSlotCount || isAutomation || isReview),
              let index = targetSlots.firstIndex(where: { $0.slotId == Int(requestedSlot) })
        else {
            let validRanges = "1...\(Self.workerSlotCount) or \(Self.automationSlotBase)...\(Self.automationSlotBase + Self.automationSlotCount - 1) or \(Self.reviewSlotBase)...\(Self.reviewSlotBase + reviewSlotCount - 1)"
            return .failure(.internalFailure(
                "engine requested slot \(requestedSlot), valid ranges are \(validRanges)"
            ))
        }
        guard targetSlots[index].session == nil else {
            return .failure(.slotBusy(occupyingRunId: targetSlots[index].runId))
        }

        let slotId: Int
        if isReview {
            slotId = reviewSlots[index].slotId
        } else if isAutomation {
            slotId = automationSlots[index].slotId
        } else {
            slotId = slots[index].slotId
        }

        let launchSpec = TerminalLaunchSpec(
            fontSize: 10.0,
            workingDirectory: request.workspacePath,
            initialInput: request.initialInput,
            env: request.env
        )
        let session = TerminalPaneSession(
            id: "run-\(request.runId)",
            role: .worker(slot: slotId),
            launchSpec: launchSpec,
            paneMonitorSpec: request.paneMonitor ?? .claudeDefault
        )
        if isReview {
            reviewSlots[index].session = session
            reviewSlots[index].runId = request.runId
            reviewSlots[index].summary = request.summary
            reviewSlots[index].taskTitle = request.taskTitle
        } else if isAutomation {
            automationSlots[index].session = session
            automationSlots[index].runId = request.runId
            automationSlots[index].summary = request.summary
            automationSlots[index].taskTitle = request.taskTitle
        } else {
            slots[index].session = session
            slots[index].runId = request.runId
            slots[index].summary = request.summary
            slots[index].taskTitle = request.taskTitle
        }

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
    func detachWorkerPane(slotId: Int) -> EngineReleaseResult {
        clearWorkerPane(slotId: slotId)
    }

    private func clearWorkerPane(slotId: Int) -> EngineReleaseResult {
        let isAutomation = Self.automationSlotRange.contains(slotId)
        let isReview = reviewSlotRange.contains(slotId)
        var targetSlots = isReview ? reviewSlots : (isAutomation ? automationSlots : slots)
        guard let index = targetSlots.firstIndex(where: { $0.slotId == slotId }) else {
            return .failure(.unknownSlot)
        }
        guard let session = targetSlots[index].session else {
            return .failure(.unknownSlot)
        }

        // Mark released before nil-ing the slot so a display-change retry
        // racing this release (see `GhosttyTerminalHostView.attemptSurfaceCreation`)
        // can't create a fresh surface and spawn a duplicate `claude` for the
        // run the engine just gave up on.
        session.markReleased()

        targetSlots[index].session = nil
        targetSlots[index].runId = nil
        targetSlots[index].summary = nil
        targetSlots[index].taskTitle = nil
        // Re-roll the idle flavor so consecutive idle bouts on the same
        // slot don't show the same line — fresh recreation each time
        // the crew member clocks out.
        targetSlots[index].idleFlavorCycle &+= 1
        if isReview {
            reviewSlots = targetSlots
        } else if isAutomation {
            automationSlots = targetSlots
        } else {
            slots = targetSlots
        }
        return .success
    }

    /// Report every slot currently hosting a session, across all three
    /// pools, regardless of whether the engine has a live-tracked run
    /// for it. Answers `EngineRequestKind.listHostedPanes` — the
    /// engine diffs this against its own live-worker registry to
    /// surface "husk" panes for `bossctl agents list --all`.
    func listHostedPanes() -> [EngineHostedPaneEntry] {
        (slots + automationSlots + reviewSlots).compactMap { slot in
            guard slot.session != nil, let runId = slot.runId else { return nil }
            return EngineHostedPaneEntry(
                slotId: slot.slotId,
                runId: runId,
                summary: slot.summary,
                taskTitle: slot.taskTitle
            )
        }
    }

    /// Resolve the foreground pid of the pty hosting `session`, or
    /// `nil` if the session never reached the point of having one
    /// (surface not yet attached, or the child already exited). Reads
    /// `ghostty_surface_foreground_pid`, which returns whatever pid is
    /// currently the foreground process group leader on the controlling
    /// tty — typically `claude` while a turn is in flight, or the shell
    /// between turns. Signalling that pid's process group reaches every
    /// descendant `claude` spawned, which is the killing radius we
    /// want.
    private func foregroundPid(for session: TerminalPaneSession) -> pid_t? {
        guard let host = session.hostView, let surface = host.surface else {
            return nil
        }
        let raw = ghostty_surface_foreground_pid(surface)
        guard raw > 0, raw <= UInt64(pid_t.max) else { return nil }
        return pid_t(raw)
    }

    /// Type text into the slot's libghostty surface and submit it as
    /// if the user had pasted the body and pressed Return. Used for
    /// probe injection (Stop-boundary text from the engine), `bossctl
    /// agents send`, and the macOS app's intervene affordance.
    ///
    /// The submit step happens inside `submitText` — see its docstring
    /// for why a trailing `\n` inside the payload is not enough to
    /// land the prompt: libghostty's paste path delivers control
    /// characters as input-field content, not as a keystroke.
    func sendToPane(slotId: Int, text: String, expectedDriverBinary: String) -> EngineSendResult {
        let targetSlots = reviewSlotRange.contains(slotId) ? reviewSlots : (Self.automationSlotRange.contains(slotId) ? automationSlots : slots)
        guard let index = targetSlots.firstIndex(where: { $0.slotId == slotId }) else {
            return .failure(.unknownSlot)
        }
        guard let session = targetSlots[index].session else {
            return .failure(.unknownSlot)
        }
        guard let host = session.hostView else {
            return .failure(.internalFailure("pane has no live surface"))
        }
        let liveness = foregroundLiveness(for: session)
        if let error = Self.driverInputError(
            expectedDriverBinary: expectedDriverBinary,
            foregroundPidIsAlive: liveness.isAlive,
            foregroundProcessName: liveness.name
        ) {
            return .failure(error)
        }
        host.submitText(text)
        return .success
    }

    /// Freshly resolve foreground liveness immediately before injecting
    /// text. A surface can remain alive after its agent returns to zsh, so a
    /// live surface or a live PTY alone is never permission to type into it.
    ///
    /// `isAlive` is the actual gate: `false` means no live process could be
    /// found on the surface's controlling tty at all — genuine death
    /// evidence. `name` is diagnostics only, for
    /// [`EngineSendError.driverExited`] — see [`Self.driverInputError`] for
    /// why it is not compared against the expected driver binary. `name`
    /// can legitimately be `nil` even when `isAlive` is `true`: `proc_name`
    /// can transiently fail (an EPERM/ESRCH race, or a name the kernel
    /// won't report) on a pid that is very much alive, and that must not be
    /// conflated with "no process" — doing so would reap a healthy worker.
    private func foregroundLiveness(for session: TerminalPaneSession) -> (isAlive: Bool, name: String?) {
        guard let pid = foregroundPid(for: session), pidIsAlive(Int32(pid)) else {
            return (false, nil)
        }
        var buffer = [CChar](repeating: 0, count: Int(MAXPATHLEN))
        guard proc_name(pid, &buffer, UInt32(buffer.count)) > 0 else {
            return (true, nil)
        }
        let end = buffer.firstIndex(of: 0) ?? buffer.endIndex
        return (true, String(decoding: buffer[..<end].map { UInt8(bitPattern: $0) }, as: UTF8.self))
    }

    /// Pure decision helper so the no-shell-input contract is testable without
    /// a real Ghostty surface.
    ///
    /// An empty `expectedDriverBinary` (protocol skew: an older or
    /// malformed engine omitted the field) is refused non-terminally —
    /// `.internalFailure`, not `.driverExited` — because it says nothing
    /// about whether the worker is alive; concluding death from "the engine
    /// did not tell us what to expect" would orphan every live worker the
    /// app writes to on a single bad frame.
    ///
    /// Deliberately does NOT compare `foregroundProcessName` against
    /// `expectedDriverBinary` by name. `ghostty_surface_foreground_pid`
    /// (see `foregroundPid`'s docstring) legitimately returns something
    /// other than the driver's own pid while the driver is alive and
    /// running a foreground child (e.g. a `bazel build` a tool call
    /// shelled out to) — the same signal `TmuxWorkerTerminalInspector`
    /// carries as a diagnostic only on the tmux path (a differing
    /// foreground command is not evidence of death, and
    /// `classify_semantic_staleness` does not consult it for health).
    /// `proc_name`'s kernel accounting name is also
    /// unreliable for this comparison: it names whatever was exec'd (an
    /// interpreter or shim for a wrapped CLI), not necessarily
    /// `DriverDescriptor.binary`. The only signal trustworthy enough to
    /// terminalize a run is refused here: no live process at all on the
    /// controlling tty (`foregroundPidIsAlive == false`) — the surface
    /// genuinely has nothing running in it.
    static func driverInputError(
        expectedDriverBinary: String,
        foregroundPidIsAlive: Bool,
        foregroundProcessName: String?
    ) -> EngineSendError? {
        guard !expectedDriverBinary.isEmpty else {
            return .internalFailure("engine did not supply expected_driver_binary")
        }
        guard foregroundPidIsAlive else {
            return .driverExited(
                expectedDriverBinary: expectedDriverBinary,
                observedProcess: foregroundProcessName
            )
        }
        return nil
    }

    /// Bring the slot's libghostty surface to first responder and
    /// raise the host window. Mirrors the user-click path in
    /// `GhosttyTerminalHostView.mouseDown` (which also calls
    /// `makeFirstResponder(self)`), then activates the application so
    /// the window is visible if it was minimised or behind another
    /// app. Used by `bossctl agents focus`.
    func focusWorkerPane(slotId: Int) -> EngineFocusResult {
        let targetSlots = reviewSlotRange.contains(slotId) ? reviewSlots : (Self.automationSlotRange.contains(slotId) ? automationSlots : slots)
        guard let index = targetSlots.firstIndex(where: { $0.slotId == slotId }) else {
            return .failure(.unknownSlot)
        }
        guard let session = targetSlots[index].session else {
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

    /// Deliver an Esc keystroke to the slot's libghostty surface —
    /// equivalent to the human pressing Esc with the pane focused.
    /// Routes through the same `ghostty_surface_key` path used by
    /// `keyDown(with:)`, so libghostty's keymap translation produces
    /// the right ESC byte sequence in the pty (and Claude treats it
    /// as an in-flight-turn cancel). Used by `bossctl agents
    /// interrupt`.
    func interruptWorkerPane(slotId: Int) -> EngineInterruptResult {
        let targetSlots = reviewSlotRange.contains(slotId) ? reviewSlots : (Self.automationSlotRange.contains(slotId) ? automationSlots : slots)
        guard let index = targetSlots.firstIndex(where: { $0.slotId == slotId }) else {
            return .failure(.unknownSlot)
        }
        guard let session = targetSlots[index].session else {
            return .failure(.unknownSlot)
        }
        guard let host = session.hostView else {
            return .failure(.internalFailure("pane has no live surface"))
        }
        host.sendInterrupt()
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
    /// Bumped every time the slot re-enters idle so the flavor line
    /// changes between idle bouts; kept stable for the lifetime of a
    /// single bout so renders don't flicker.
    var idleFlavorCycle: Int = 0

    var id: Int { slotId }

    static func == (lhs: WorkerSlot, rhs: WorkerSlot) -> Bool {
        lhs.slotId == rhs.slotId
            && lhs.runId == rhs.runId
            && lhs.summary == rhs.summary
            && lhs.taskTitle == rhs.taskTitle
            && lhs.idleFlavorCycle == rhs.idleFlavorCycle
            && lhs.session === rhs.session
    }
}

extension EngineSpawnRequest {
    /// Viewer-launch parameters derived from `AttachWorkerPane`.
    ///
    /// `workspacePath` is the tmux client's working directory (the current
    /// user's home), not the worker workspace — the attach RPC does not
    /// carry that path. Spawn diagnostics must record `sessionName` and
    /// `tmuxSocketPath` instead of treating this directory as `workspace_path`.
    init(attaching request: EngineAttachRequest) {
        self.init(
            runId: request.runId,
            workspacePath: FileManager.default.homeDirectoryForCurrentUser.path,
            slotId: request.slotId,
            initialInput: "exec tmux -S \(bossShellQuote(request.tmuxSocketPath)) attach-session -t \(bossShellQuote(request.sessionName))\n",
            env: [],
            summary: request.summary,
            taskTitle: request.taskTitle,
            paneMonitor: .claudeDefault,
            sessionName: request.sessionName,
            tmuxSocketPath: request.tmuxSocketPath
        )
    }
}
