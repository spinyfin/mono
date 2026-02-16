import Foundation

struct PendingPermission: Identifiable {
    let id: String
    let title: String
}

@MainActor
final class ChatViewModel: ObservableObject {
    @Published var messages: [ChatMessage] = [
        ChatMessage(role: .system, text: "Starting boss frontend…"),
    ]
    @Published var draft: String = ""
    @Published var isConnected: Bool = false
    @Published var isSending: Bool = false
    @Published var pendingPermission: PendingPermission?

    private let engine: EngineClient
    private let processController = EngineProcessController()
    private var activeAssistantMessageID: UUID?
    private var permissionQueue: [PendingPermission] = []

    init(
        socketPath: String = ProcessInfo.processInfo.environment["BOSS_SOCKET_PATH"]
            ?? "/tmp/boss-engine.sock"
    ) {
        engine = EngineClient(socketPath: socketPath)

        processController.onOutputLine = { [weak self] line in
            self?.messages.append(ChatMessage(role: .system, text: line))
        }

        engine.onEvent = { [weak self] event in
            self?.handle(event)
        }

        let autostart = ProcessInfo.processInfo.environment["BOSS_ENGINE_AUTOSTART"] != "0"
        if autostart {
            do {
                try processController.start(socketPath: socketPath)
                messages.append(ChatMessage(role: .system, text: "Launching engine process…"))
            } catch {
                messages.append(
                    ChatMessage(role: .system, text: "Failed to launch engine: \(error.localizedDescription)")
                )
            }

            Task {
                try? await Task.sleep(for: .seconds(1.0))
                engine.start()
            }
        } else {
            messages.append(
                ChatMessage(
                    role: .system,
                    text: "Auto-start disabled. Connects to an external engine socket."
                )
            )
            engine.start()
        }
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

    func respondToPendingPermission(granted: Bool) {
        guard let pendingPermission else {
            return
        }

        engine.sendPermissionResponse(id: pendingPermission.id, granted: granted)
        messages.append(
            ChatMessage(
                role: .system,
                text: "[permission] \(granted ? "allowed" : "denied"): \(pendingPermission.title)"
            )
        )

        self.pendingPermission = nil
        showNextPermissionIfNeeded()
    }

    private func handle(_ event: EngineEvent) {
        switch event {
        case .connected:
            isConnected = true
            messages.append(ChatMessage(role: .system, text: "Connected to engine socket."))
        case .disconnected:
            isConnected = false
            isSending = false
            activeAssistantMessageID = nil
            messages.append(ChatMessage(role: .system, text: "Disconnected from engine socket."))
        case .chunk(let text):
            appendAssistantChunk(text)
        case .done(let stopReason):
            isSending = false
            activeAssistantMessageID = nil
            messages.append(ChatMessage(role: .system, text: "[done] \(stopReason)"))
        case .toolCall(let name, let status):
            messages.append(ChatMessage(role: .system, text: "[tool] \(name) (\(status))"))
        case .permissionRequest(let id, let title):
            enqueuePermission(id: id, title: title)
        case .error(let message):
            isSending = false
            activeAssistantMessageID = nil
            messages.append(ChatMessage(role: .system, text: "[error] \(message)"))
        }
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
