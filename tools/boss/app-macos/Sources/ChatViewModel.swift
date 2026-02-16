import Foundation

struct PendingPermission: Identifiable {
    let id: String
    let title: String
}

@MainActor
final class ChatViewModel: ObservableObject {
    @Published var messages: [ChatMessage] = []
    @Published var draft: String = ""
    @Published var isConnected: Bool = false
    @Published var isSending: Bool = false
    @Published var pendingPermission: PendingPermission?

    private let engine: EngineClient
    private let processController = EngineProcessController()
    private let socketPath: String
    private let showSystemMessages: Bool
    private var didStart = false
    private var activeAssistantMessageID: UUID?
    private var permissionQueue: [PendingPermission] = []

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

        messages.append(ChatMessage(role: .user, text: trimmed))
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
        }
        engine.start()
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
        case .permissionRequest(let id, let title):
            enqueuePermission(id: id, title: title)
        case .error(let message):
            isSending = false
            activeAssistantMessageID = nil
            appendSystemMessage("[error] \(message)", alwaysShow: true)
        }
    }

    private func appendSystemMessage(_ text: String, alwaysShow: Bool = false) {
        guard alwaysShow || showSystemMessages else {
            return
        }
        messages.append(ChatMessage(role: .system, text: text))
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
        if let id = activeAssistantMessageID,
            let index = messages.firstIndex(where: { $0.id == id })
        {
            messages[index].text += text
            return
        }

        let message = ChatMessage(role: .assistant, text: text)
        activeAssistantMessageID = message.id
        messages.append(message)
    }
}
