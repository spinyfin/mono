import Foundation

struct PendingPermission: Identifiable {
    let id: String
    let title: String
}

@MainActor
final class ChatViewModel: ObservableObject {
    @Published var timeline: [TranscriptItem] = []
    @Published var draft: String = ""
    @Published var isConnected: Bool = false
    @Published var isSending: Bool = false
    @Published var pendingPermission: PendingPermission?

    private let engine: EngineClient
    private let processController = EngineProcessController()
    private let socketPath: String
    private let showSystemMessages: Bool
    private var didStart = false
    private var didStartEngine = false
    private var hasConnectedOnce = false
    private var activeAssistantMessageID: UUID?
    private var permissionQueue: [PendingPermission] = []
    private var terminalEntryIndexByID: [String: Int] = [:]

    private let maxTerminalOutputChars = 200_000

    init(
        socketPath: String = ProcessInfo.processInfo.environment["BOSS_SOCKET_PATH"]
            ?? "/tmp/boss-engine.sock"
    ) {
        self.socketPath = socketPath
        let showSystem = ProcessInfo.processInfo.environment["BOSS_SHOW_SYSTEM_MESSAGES"] ?? ""
        showSystemMessages = showSystem == "1" || showSystem.lowercased() == "true"
        engine = EngineClient(socketPath: socketPath)

        processController.onOutputLine = { [weak self] line in
            self?.appendSystemMessage(line)
        }

        engine.onEvent = { [weak self] event in
            self?.handle(event)
        }

        appendSystemMessage("Starting boss frontend…")
    }

    deinit {
        processController.stop()
        engine.stop()
    }

    func sendDraft() {
        let trimmed = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            return
        }

        appendMessage(role: .user, text: trimmed)
        isSending = true
        activeAssistantMessageID = nil
        engine.sendPrompt(trimmed)
        draft = ""
    }

    func startIfNeeded() {
        guard !didStart else {
            return
        }
        didStart = true

        let autostart = ProcessInfo.processInfo.environment["BOSS_ENGINE_AUTOSTART"] != "0"
        if autostart {
            appendSystemMessage("Ensuring engine process is running…")
            let socketPath = self.socketPath
            let processController = self.processController
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                do {
                    try processController.start(socketPath: socketPath)
                    DispatchQueue.main.async {
                        self?.startEngineIfNeeded()
                    }
                } catch {
                    DispatchQueue.main.async {
                        self?.appendSystemMessage(
                            "Failed to launch engine: \(error.localizedDescription)",
                            alwaysShow: true
                        )
                    }
                }
            }
        } else {
            appendSystemMessage(
                "Auto-start disabled. Connects to an external engine socket."
            )
            startEngineIfNeeded()
        }
    }

    func respondToPendingPermission(granted: Bool) {
        guard let pendingPermission else {
            return
        }

        engine.sendPermissionResponse(id: pendingPermission.id, granted: granted)
        appendSystemMessage(
            "[permission] \(granted ? "allowed" : "denied"): \(pendingPermission.title)"
        )

        self.pendingPermission = nil
        showNextPermissionIfNeeded()
    }

    private func handle(_ event: EngineEvent) {
        switch event {
        case .connected:
            isConnected = true
            hasConnectedOnce = true
            appendSystemMessage("Connected to engine socket.")
        case .disconnected:
            isConnected = false
            isSending = false
            activeAssistantMessageID = nil
            appendSystemMessage("Disconnected from engine socket.")
        case .chunk(let text):
            appendAssistantChunk(text)
        case .done(let stopReason):
            isSending = false
            activeAssistantMessageID = nil
            appendSystemMessage("[done] \(stopReason)")
        case .toolCall(let name, let status):
            appendSystemMessage("[tool] \(name) (\(status))")
        case .terminalStarted(let id, let title, let command, let cwd):
            // Once a terminal starts, subsequent assistant chunks should render after it.
            activeAssistantMessageID = nil
            upsertTerminalActivity(id: id, title: title, command: command, cwd: cwd)
        case .terminalOutput(let id, let text):
            appendTerminalOutput(id: id, text: text)
        case .terminalDone(let id, let exitCode, let signal):
            completeTerminalActivity(id: id, exitCode: exitCode, signal: signal)
        case .permissionRequest(let id, let title):
            enqueuePermission(id: id, title: title)
        case .error(let message):
            isSending = false
            activeAssistantMessageID = nil
            if shouldSuppressSocketStartupError(message) {
                return
            }
            appendSystemMessage("[error] \(message)", alwaysShow: true)
        }
    }

    private func startEngineIfNeeded() {
        guard !didStartEngine else {
            return
        }
        didStartEngine = true
        engine.start()
    }

    private func shouldSuppressSocketStartupError(_ message: String) -> Bool {
        guard !showSystemMessages, !hasConnectedOnce else {
            return false
        }

        if message.hasPrefix("socket failed:") || message.hasPrefix("socket waiting:") {
            return true
        }

        return false
    }

    private func appendMessage(role: ChatRole, text: String) {
        timeline.append(.message(ChatMessage(role: role, text: text)))
    }

    private func appendSystemMessage(_ text: String, alwaysShow: Bool = false) {
        guard alwaysShow || showSystemMessages else {
            return
        }
        appendMessage(role: .system, text: text)
    }

    private func enqueuePermission(id: String, title: String) {
        let request = PendingPermission(id: id, title: title)
        if pendingPermission == nil {
            pendingPermission = request
        } else {
            permissionQueue.append(request)
        }
    }

    private func showNextPermissionIfNeeded() {
        guard pendingPermission == nil, !permissionQueue.isEmpty else {
            return
        }

        pendingPermission = permissionQueue.removeFirst()
    }

    private func appendAssistantChunk(_ text: String) {
        if let id = activeAssistantMessageID, let index = messageIndex(for: id) {
            guard case .message(var message) = timeline[index] else {
                return
            }
            message.text += text
            timeline[index] = .message(message)
            return
        }

        let message = ChatMessage(role: .assistant, text: text)
        activeAssistantMessageID = message.id
        timeline.append(.message(message))
    }

    private func messageIndex(for id: UUID) -> Int? {
        timeline.firstIndex { item in
            guard case .message(let message) = item else {
                return false
            }
            return message.id == id
        }
    }

    private func upsertTerminalActivity(id: String, title: String, command: String, cwd: String?) {
        let index = ensureTerminalActivity(id: id)
        guard case .terminal(var activity) = timeline[index] else {
            return
        }

        activity.title = title
        if !command.isEmpty {
            activity.command = command
        }
        if let cwd {
            activity.cwd = cwd
        }
        activity.status = "Running…"
        timeline[index] = .terminal(activity)
    }

    private func appendTerminalOutput(id: String, text: String) {
        let index = ensureTerminalActivity(id: id)
        guard case .terminal(var activity) = timeline[index] else {
            return
        }

        activity.output += text
        if activity.output.count > maxTerminalOutputChars {
            let overflow = activity.output.count - maxTerminalOutputChars
            activity.output.removeFirst(overflow)
        }
        timeline[index] = .terminal(activity)
    }

    private func completeTerminalActivity(id: String, exitCode: Int?, signal: String?) {
        let index = ensureTerminalActivity(id: id)
        guard case .terminal(var activity) = timeline[index] else {
            return
        }

        if let exitCode {
            if exitCode == 0 {
                activity.status = "Done"
            } else {
                activity.status = "Failed (exit \(exitCode))"
            }
        } else if let signal, !signal.isEmpty {
            activity.status = "Terminated (signal \(signal))"
        } else {
            activity.status = "Done"
        }

        timeline[index] = .terminal(activity)
    }

    private func ensureTerminalActivity(id: String) -> Int {
        if let index = terminalEntryIndexByID[id],
            index < timeline.count,
            case .terminal = timeline[index]
        {
            return index
        }

        let activity = TerminalActivity(
            id: id,
            title: "Terminal command",
            command: "",
            cwd: nil,
            output: "",
            status: "Running…"
        )
        let index = timeline.count
        timeline.append(.terminal(activity))
        terminalEntryIndexByID[id] = index
        return index
    }
}
