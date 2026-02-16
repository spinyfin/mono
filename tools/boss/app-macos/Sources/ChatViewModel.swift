import Foundation

@MainActor
final class ChatViewModel: ObservableObject {
    @Published var messages: [ChatMessage] = [
        ChatMessage(
            role: .system,
            text: "Connect the engine with: bazel run //tools/boss/engine:engine -- --mode=server"
        ),
    ]
    @Published var draft: String = ""
    @Published var isConnected: Bool = false
    @Published var isSending: Bool = false

    private let engine: EngineClient
    private var activeAssistantMessageID: UUID?

    init(
        socketPath: String = ProcessInfo.processInfo.environment["BOSS_SOCKET_PATH"]
            ?? "/tmp/boss-engine.sock"
    ) {
        engine = EngineClient(socketPath: socketPath)
        engine.onEvent = { [weak self] event in
            self?.handle(event)
        }
        engine.start()
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
            messages.append(ChatMessage(role: .system, text: "[permission] auto-allowing: \(title)"))
            engine.sendPermissionResponse(id: id, granted: true)
        case .error(let message):
            isSending = false
            activeAssistantMessageID = nil
            messages.append(ChatMessage(role: .system, text: "[error] \(message)"))
        }
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
