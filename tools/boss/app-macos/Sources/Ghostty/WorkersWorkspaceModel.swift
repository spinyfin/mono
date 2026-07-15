import AppKit
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
    static let automationSlotCount = 6
    static let automationSlotBase = workerSlotCount + 1   // 17
    static let automationSlotRange = automationSlotBase...(automationSlotBase + automationSlotCount - 1)  // 17...22
    /// Review pool occupies slot IDs immediately above the automation pool.
    /// The count is set dynamically via configureSlots(workerCount:automationCount:reviewCount:)
    /// when the engine pushes EnginePoolConfig on RegisterAppSession, so the
    /// app never independently hardcodes a value that drifts from the engine.
    /// The initial value of 8 matches DEFAULT_REVIEW_POOL_SIZE in coordinator.rs
    /// and ensures the slot grid renders correctly before the first pool-config
    /// push arrives (covering the unlikely race of a SpawnWorkerPane before
    /// EnginePoolConfig, and preventing an empty grid on first launch).
    static let reviewSlotBase = automationSlotBase + automationSlotCount   // 23

    /// Instance-level review slot count, kept in sync with the engine's live
    /// pool config. Published so the pool-picker header re-renders whenever
    /// the engine reports a pool size change on reconnect.
    @Published private(set) var reviewSlotCount: Int = 8

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
        self.reviewSlots = (Self.reviewSlotBase...(Self.reviewSlotBase + 8 - 1)).map { slot in
            WorkerSlot(slotId: slot, idleFlavorCycle: Int.random(in: 0...10_000))
        }
    }

    /// Called after a worker pane's libghostty surface attaches and the
    /// shell pid becomes available. `ContentView` installs this closure to
    /// forward the pid to the engine via `sendUpdateWorkerShellPid`. The
    /// `runId` is the raw execution id (without the "run-" session prefix).
    var onShellPidAvailable: ((String, Int32) -> Void)?

    /// Called when a worker pane dies before the engine could observe it
    /// any other way — its surface never attached (`onSurfaceFailed`) or
    /// its shell process exited (`onChildExited`, which only worker panes
    /// wire up; the Boss pane instead restarts itself). `ContentView`
    /// installs this closure to forward the death to the engine via
    /// `sendWorkerPaneDied` so reconciliation fires immediately instead of
    /// waiting for the periodic dead-pid sweep. The `runId` is the raw
    /// execution id (without the "run-" session prefix).
    var onPaneDied: ((String) -> Void)?

    /// Called when a worker pane's libghostty surface FAILS to create so no
    /// shell ever comes up (the post-sleep "no active display" condition).
    /// `ContentView` installs this closure to NACK the engine via
    /// `sendReportWorkerSpawnFailed`, so it fails the spawn fast instead of
    /// waiting out the 60s spawn-ack timeout. `runId` is the raw execution id;
    /// `reason` is a short human-readable cause.
    var onSpawnFailed: ((String, String) -> Void)?

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

    /// Host a worker pane in the slot the engine has claimed for
    /// this worker (`request.slotId`). The engine is the source of
    /// truth for slot allocation: this method honors the requested
    /// slot or fails — it never picks a different slot.
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
    func spawnWorkerPane(_ request: EngineSpawnRequest) -> EngineSpawnResult {
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
        // Durable, execution-id-correlatable record that we honored the spawn
        // request — logged BEFORE the asynchronous surface creation so a spawn
        // that never progresses to an attached surface or a failure is still
        // visible app-side (the gap the 2026-07-05 post-wake incident hit).
        SpawnDiagnosticsLog.shared.spawnRequested(
            runId: request.runId,
            slotId: slotId,
            workspacePath: request.workspacePath
        )

        let launchSpec = TerminalLaunchSpec(
            fontSize: 10.0,
            workingDirectory: request.workspacePath,
            initialInput: request.initialInput,
            env: request.env
        )
        let session = TerminalPaneSession(
            id: "run-\(request.runId)",
            role: .worker(slot: slotId),
            launchSpec: launchSpec
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

        // Return shell_pid 0 now — the libghostty surface is created
        // asynchronously by SwiftUI after this RPC returns. Once the surface
        // attaches, onSurfaceAttached fires and we read foregroundPid (which
        // calls ghostty_surface_foreground_pid) to get the real shell pid,
        // then forward it to the engine via update_worker_shell_pid.
        let capturedRunId = request.runId
        let capturedSlotId = slotId
        session.onSurfaceAttached = { [weak self, weak session] in
            guard let self = self else { return }
            let pid = session?.shellPid ?? 0
            if pid > 0 {
                SpawnDiagnosticsLog.shared.surfaceAttached(runId: capturedRunId, slotId: capturedSlotId, shellPid: pid)
                WorkerProcessPriority.applyBackgroundPriority(toShellPid: pid, runId: capturedRunId)
                self.onShellPidAvailable?(capturedRunId, pid)
            } else {
                // Shell may not have called tcsetpgrp yet — retry after a
                // short delay to let it become the foreground process group.
                Task { @MainActor [weak self, weak session] in
                    try? await Task.sleep(nanoseconds: 250_000_000) // 250ms
                    guard let self = self else { return }
                    let retryPid = session?.shellPid ?? 0
                    guard retryPid > 0 else { return }
                    SpawnDiagnosticsLog.shared.surfaceAttached(
                        runId: capturedRunId,
                        slotId: capturedSlotId,
                        shellPid: retryPid
                    )
                    WorkerProcessPriority.applyBackgroundPriority(toShellPid: retryPid, runId: capturedRunId)
                    self.onShellPidAvailable?(capturedRunId, retryPid)
                }
            }
        }
        // Report the pane's death to the engine the moment the app itself
        // observes it — either the surface never attached at all, or the
        // shell process exited — instead of waiting for the periodic
        // dead-pid sweep (up to 60s) or an app restart to notice.
        session.onSurfaceFailed = { [weak self] in
            self?.onPaneDied?(capturedRunId)
        }
        session.onChildExited = { [weak self] in
            self?.onPaneDied?(capturedRunId)
        }
        // Fail-fast NACK: if the libghostty surface never comes up (the
        // post-sleep "no active display" condition), tell the engine at once
        // instead of leaving it to time out after 60s. Also mirror the failure
        // into the durable spawn diagnostics keyed by execution id.
        session.onSurfaceCreationFailed = { [weak self] reason in
            SpawnDiagnosticsLog.shared.surfaceFailed(runId: capturedRunId, reason: reason)
            self?.onSpawnFailed?(capturedRunId, reason)
        }

        return .success(slotId: slotId, shellPid: 0)
    }

    /// Release a previously allocated slot.
    ///
    /// Niling the session lets SwiftUI dismantle the libghostty surface
    /// via `GhosttyTerminalHostView.deinit` — which clears focus and
    /// calls `ghostty_surface_free`, freeing the PTY, scrollback and
    /// GPU resources. That alone is insufficient to reap the worker:
    /// `claude` runs as a descendant of the pty's foreground process
    /// group, and closing the master fd only delivers `SIGHUP`, which
    /// node-based processes commonly ignore.
    ///
    /// Incident 001 (cross-workspace PR-detection killed running
    /// workers) revealed that the engine considered the worker dead as
    /// soon as the IPC came back successful, but the `claude` process
    /// kept running invisibly against the workspace. Here we (a)
    /// snapshot the foreground pid from the surface *before* we nil the
    /// session so SwiftUI's teardown can't race us, and (b) escalate
    /// SIGTERM → SIGKILL through [`WorkerProcessKiller`] on the worker's
    /// process group, matching the engine-side `signal_shell_pids`
    /// shape (engine.app.shutdown_workers uses the same ladder for the
    /// shutdown-path fallback).
    ///
    /// The SIGTERM is fired synchronously before this method returns
    /// — so by the time the engine sees `Ok(ReleaseWorkerPaneResult)`,
    /// the worker has at minimum been asked to exit. The SIGKILL
    /// escalation runs on a detached task so we don't block the IPC
    /// dispatcher's main-actor turn for `killGraceSeconds` (5s by
    /// default, which would itself blow the engine's 5s round-trip
    /// budget).
    func releaseWorkerPane(slotId: Int, killGraceSeconds: UInt32) -> EngineReleaseResult {
        let isAutomation = Self.automationSlotRange.contains(slotId)
        let isReview = reviewSlotRange.contains(slotId)
        var targetSlots = isReview ? reviewSlots : (isAutomation ? automationSlots : slots)
        guard let index = targetSlots.firstIndex(where: { $0.slotId == slotId }) else {
            return .failure(.unknownSlot)
        }
        guard let session = targetSlots[index].session else {
            return .failure(.unknownSlot)
        }

        let foregroundPid = foregroundPid(for: session)
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

        if let pid = foregroundPid {
            Task.detached(priority: .userInitiated) {
                await WorkerProcessKiller.killForegroundProcessTree(
                    pid: pid,
                    graceSeconds: killGraceSeconds
                )
            }
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
    func sendToPane(slotId: Int, text: String) -> EngineSendResult {
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
        host.submitText(text)
        return .success
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
