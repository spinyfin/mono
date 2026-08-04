import Foundation
import GhosttyKit

/// Driver-supplied (or Claude-default) substrings the pane monitor uses
/// to screen-scrape a GhosttyKit viewport. Mirrors
/// `boss_protocol::PaneMonitorSpec` on the spawn RPC.
struct PaneMonitorSpec: Equatable, Sendable {
    let agentMarkers: [String]
    let busyMarkers: [String]
    let startingMarkers: [String]
    let promptPrefixes: [String]
    let idleDebouncePolls: Int

    /// Historical Claude literals from the pre-spec app. Used when the
    /// spawn message carries no `pane_monitor` (older engine, or a
    /// driver that declares no spec) so existing paths stay identical.
    static let claudeDefault = PaneMonitorSpec(
        agentMarkers: ["Claude Code", "auto mode on", "/effort"],
        busyMarkers: ["esc to interrupt"],
        startingMarkers: ["Accessing workspace:", "Quick safety check:"],
        promptPrefixes: ["❯"],
        idleDebouncePolls: 2
    )

    /// Parse a wire dict from `SpawnWorkerPaneInput.pane_monitor`, or
    /// return `claudeDefault` when the field is absent/malformed.
    static func fromWire(_ dict: [String: Any]?) -> PaneMonitorSpec {
        guard let dict else { return .claudeDefault }
        let agent = dict["agent_markers"] as? [String] ?? []
        let busy = dict["busy_markers"] as? [String] ?? []
        let starting = dict["starting_markers"] as? [String] ?? []
        let prompts = dict["prompt_prefixes"] as? [String] ?? []
        let debounceRaw = dict["idle_debounce_polls"]
        let debounce: Int
        if let n = debounceRaw as? Int {
            debounce = n
        } else if let n = debounceRaw as? NSNumber {
            debounce = n.intValue
        } else {
            debounce = claudeDefault.idleDebouncePolls
        }
        // An empty agent-marker list would permanently pin notDetected;
        // treat a hollow payload the same as absent.
        guard !agent.isEmpty else { return .claudeDefault }
        return PaneMonitorSpec(
            agentMarkers: agent,
            busyMarkers: busy,
            startingMarkers: starting,
            promptPrefixes: prompts.isEmpty ? claudeDefault.promptPrefixes : prompts,
            idleDebouncePolls: max(1, debounce)
        )
    }
}

enum PaneMonitorState: Equatable {
    case unavailable
    case notDetected
    case ready
    case working

    var label: String {
        switch self {
        case .unavailable:
            "Agent Unknown"
        case .notDetected:
            "Not Detected"
        case .ready:
            "Ready"
        case .working:
            "Working"
        }
    }
}

struct TerminalLaunchSpec {
    let fontSize: Float32
    let workingDirectory: String
    let initialInput: String
    /// Env vars to set on the spawned shell, layered over the app's
    /// inherited env. The engine builds a strict allowlist for worker
    /// spawns (sanitized PATH excluding `bossctl`, plus
    /// `BOSS_EVENTS_SOCKET` / `BOSS_LEASE_ID`); the Boss pane passes
    /// `bossSessionEnv()` to set `BOSS_BIN_DIR`, `BOSS_BIN`, and an
    /// initial PATH prepend; ad-hoc test panes pass an empty array.
    let env: [(String, String)]

    init(
        fontSize: Float32,
        workingDirectory: String,
        initialInput: String,
        env: [(String, String)] = []
    ) {
        self.fontSize = fontSize
        self.workingDirectory = workingDirectory
        self.initialInput = initialInput
        self.env = env
    }
}

struct PaneMonitorSnapshot {
    let tail: String
    let agentVisible: Bool
    let busy: Bool
    let promptVisible: Bool
    let promptLine: String?
    let starting: Bool
}

struct PaneMonitorTracker {
    private let idleDebouncePolls: Int
    private let promptPrefixes: [String]
    private var lastTail: String?
    private var lastPromptLine: String?
    private var turnInFlight = false
    private var stablePromptPolls = 0

    init(spec: PaneMonitorSpec = .claudeDefault) {
        self.idleDebouncePolls = max(1, spec.idleDebouncePolls)
        self.promptPrefixes = spec.promptPrefixes
    }

    mutating func reset() {
        lastTail = nil
        lastPromptLine = nil
        turnInFlight = false
        stablePromptPolls = 0
    }

    mutating func evaluate(_ snapshot: PaneMonitorSnapshot?) -> PaneMonitorState {
        guard let snapshot else {
            reset()
            return .unavailable
        }

        guard snapshot.agentVisible else {
            reset()
            return .notDetected
        }

        let tailChanged = lastTail.map { $0 != snapshot.tail } ?? false
        let promptJustSubmitted =
            !turnInFlight &&
            tailChanged &&
            promptHasInput(lastPromptLine) &&
            snapshot.promptVisible &&
            !promptHasInput(snapshot.promptLine)

        defer {
            lastTail = snapshot.tail
            lastPromptLine = snapshot.promptLine
        }

        if snapshot.busy || snapshot.starting {
            turnInFlight = true
            stablePromptPolls = 0
            return .working
        }

        if promptJustSubmitted {
            turnInFlight = true
            stablePromptPolls = 0
        }

        if snapshot.promptVisible {
            guard turnInFlight else {
                stablePromptPolls = 0
                return .ready
            }

            stablePromptPolls = tailChanged ? 1 : stablePromptPolls + 1
            if stablePromptPolls >= idleDebouncePolls {
                turnInFlight = false
                stablePromptPolls = 0
                return .ready
            }

            return .working
        }

        turnInFlight = true
        stablePromptPolls = 0
        return .working
    }

    private func promptHasInput(_ promptLine: String?) -> Bool {
        guard let promptLine else { return false }
        let trimmed = promptLine.trimmingCharacters(in: .whitespaces)
        for prefix in promptPrefixes {
            if trimmed.hasPrefix(prefix) {
                let remainder = trimmed.dropFirst(prefix.count)
                return !remainder.trimmingCharacters(in: .whitespaces).isEmpty
            }
        }
        return false
    }
}

enum PaneRole: Equatable {
    case boss
    case worker(slot: Int)

    var defaultTitle: String {
        switch self {
        case .boss: "Picard"
        case .worker(let slot): WorkerNames.name(forSlot: slot)
        }
    }
}

/// App callback that supplied a worker-pane death observation. Raw values are
/// the protocol strings sent to the engine in `worker_pane_died.reason`.
enum WorkerPaneDeathReason: String, Equatable {
    case surfaceCreationFailed = "surface_creation_failed"
    case childProcessExited = "child_process_exited"
}

@MainActor
final class TerminalPaneSession: ObservableObject, Identifiable {
    let id: String
    let role: PaneRole
    let launchSpec: TerminalLaunchSpec
    /// Driver-supplied (or Claude-default) markers for the pre-hook
    /// viewport screen-scrape. Set at spawn from
    /// `SpawnWorkerPaneInput.pane_monitor`.
    let paneMonitorSpec: PaneMonitorSpec

    @Published var displayTitle: String
    @Published var workingDirectory: String
    @Published var rendererHealthy = false
    @Published var statusMessage: String?
    @Published var terminalReady = false
    @Published var paneMonitorState: PaneMonitorState = .unavailable

    weak var hostView: GhosttyTerminalHostView?
    /// The foreground pid of this pane's PTY, or 0 when the surface is not
    /// yet live. Delegates to `GhosttyTerminalHostView.foregroundPid`.
    var shellPid: Int32 { hostView?.foregroundPid ?? 0 }
    /// Set by `WorkersWorkspaceModel.releaseWorkerPane` the instant a slot is
    /// released, before SwiftUI has necessarily torn down the host view.
    /// `GhosttyTerminalHostView.attemptSurfaceCreation` checks this so a
    /// display-change retry that fires after release (e.g. the fast-fail
    /// NACK reaped the execution while a `NSScreen` observer was still
    /// armed) can't create a fresh surface and spawn a duplicate `claude`
    /// for an execution the engine has already given up on.
    private(set) var isReleased = false
    /// One pane may produce more than one close/failure callback while its
    /// surface is dismantled. Only the first genuine death observation is
    /// reportable to the engine.
    private var paneDeathReported = false

    /// Mark this session as released. Idempotent.
    func markReleased() {
        isReleased = true
    }

    /// Atomically claim this session's one allowed pane-death report.
    /// Main-actor isolation serializes the two callback sources.
    func claimPaneDeathReport() -> Bool {
        guard !paneDeathReported else { return false }
        paneDeathReported = true
        return true
    }
    private var paneMonitorTracker: PaneMonitorTracker
    /// Called on the main actor when the pane's child process exits.
    /// Boss pane sets this to a restart closure; worker panes leave it nil.
    var onChildExited: (() -> Void)?
    /// Called on the main actor each time a libghostty surface is
    /// successfully attached to this session. Fires on initial creation
    /// and on every restart (the surface is torn down and re-created
    /// when the child exits). Boss pane uses this to re-register the
    /// Boss trust root after a restart produces a new shell pid.
    var onSurfaceAttached: (() -> Void)?
    /// Called on the main actor each time `ghostty_surface_new` returns
    /// NULL for this session (no active display, or another rejected
    /// precondition) — the pane never got a shell process at all. Worker
    /// panes use this to report the death to the engine immediately
    /// instead of waiting for the periodic dead-pid sweep; the Boss pane
    /// leaves it nil (it has no engine-tracked execution to reap).
    var onSurfaceFailed: (() -> Void)?
    /// Called on the main actor when this session's libghostty surface
    /// FAILS to create (`ghostty_surface_new` returned NULL — typically the
    /// post-sleep "no active display" condition, #800). Worker panes set
    /// this to a closure that NACKs the spawn back to the engine
    /// (`report_worker_spawn_failed`) so it fails fast instead of waiting
    /// out the 60s spawn-ack timeout, and logs a durable diagnostic. Fired
    /// at most once per session — the host view dedupes — and never for a
    /// surface that eventually succeeds. Boss pane leaves this nil.
    var onSurfaceCreationFailed: ((_ reason: String) -> Void)?


    init(
        id: String,
        role: PaneRole,
        launchSpec: TerminalLaunchSpec,
        paneMonitorSpec: PaneMonitorSpec = .claudeDefault
    ) {
        self.id = id
        self.role = role
        self.launchSpec = launchSpec
        self.paneMonitorSpec = paneMonitorSpec
        self.paneMonitorTracker = PaneMonitorTracker(spec: paneMonitorSpec)
        self.displayTitle = role.defaultTitle
        self.workingDirectory = launchSpec.workingDirectory
    }

    func setTitle(_ title: String) {
        displayTitle = title.isEmpty ? role.defaultTitle : title
    }

    func attach(hostView: GhosttyTerminalHostView) {
        self.hostView = hostView
        terminalReady = true
        onSurfaceAttached?()
    }

    func updatePaneMonitor(snapshot: PaneMonitorSnapshot?) {
        paneMonitorState = paneMonitorTracker.evaluate(snapshot)
    }
}
