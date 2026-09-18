import Foundation
import GhosttyKit

/// Driver-supplied (or Claude-default) substrings the pane monitor uses
/// to screen-scrape a GhosttyKit viewport. Mirrors
/// `boss_protocol::PaneMonitorSpec`.
struct PaneMonitorSpec: Equatable, Sendable {
    let agentMarkers: [String]
    let busyMarkers: [String]
    let startingMarkers: [String]
    let promptPrefixes: [String]
    let idleDebouncePolls: Int

    /// Historical Claude literals from the pre-spec app. Used whenever no
    /// driver-specific spec is available so existing paths stay identical.
    static let claudeDefault = PaneMonitorSpec(
        agentMarkers: ["Claude Code", "auto mode on", "/effort"],
        busyMarkers: ["esc to interrupt"],
        startingMarkers: ["Accessing workspace:", "Quick safety check:"],
        promptPrefixes: ["❯"],
        idleDebouncePolls: 2
    )
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
    /// `BOSS_EVENTS_SOCKET` / `BOSS_LEASE_ID`). The Boss pane is only a
    /// tmux client; the engine configures the detached coordinator's
    /// environment when it creates the session. Ad-hoc test panes pass an
    /// empty array.
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

@MainActor
final class TerminalPaneSession: ObservableObject, Identifiable {
    let id: String
    let role: PaneRole
    let launchSpec: TerminalLaunchSpec
    /// Driver-supplied (or Claude-default) markers for the pre-hook
    /// viewport screen-scrape.
    let paneMonitorSpec: PaneMonitorSpec

    @Published var displayTitle: String
    @Published var workingDirectory: String
    @Published var rendererHealthy = false
    @Published var statusMessage: String?
    @Published var terminalReady = false
    @Published var paneMonitorState: PaneMonitorState = .unavailable

    weak var hostView: GhosttyTerminalHostView?
    /// Set by `WorkersWorkspaceModel.clearWorkerPane` the instant a slot is
    /// detached, before SwiftUI has necessarily torn down the host view.
    /// `GhosttyTerminalHostView.attemptSurfaceCreation` checks this so a
    /// display-change retry that fires after detach (e.g. while an
    /// `NSScreen` observer was still armed) can't create a fresh surface
    /// and spawn a duplicate viewer for a slot the engine has already given
    /// up on.
    private(set) var isReleased = false

    /// Mark this session as released. Idempotent.
    func markReleased() {
        isReleased = true
    }

    private var paneMonitorTracker: PaneMonitorTracker
    /// Called on the main actor when the pane's child process exits. Only
    /// the coordinator pane uses it, to rebuild its local tmux client while
    /// the engine-owned detached coordinator session remains unaffected.
    var onChildExited: (() -> Void)?

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
    }

    func updatePaneMonitor(snapshot: PaneMonitorSnapshot?) {
        paneMonitorState = paneMonitorTracker.evaluate(snapshot)
    }
}
