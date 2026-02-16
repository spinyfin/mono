import Foundation

enum ChatRole {
    case user
    case assistant
    case system
}

struct ChatMessage: Identifiable {
    let id: UUID
    let role: ChatRole
    var text: String

    init(id: UUID = UUID(), role: ChatRole, text: String) {
        self.id = id
        self.role = role
        self.text = text
    }
}

struct TerminalActivity: Identifiable {
    let id: String
    var title: String
    var command: String
    var cwd: String?
    var output: String
    var status: String
}

enum TranscriptItem: Identifiable {
    case message(ChatMessage)
    case terminal(TerminalActivity)

    var id: String {
        switch self {
        case .message(let message):
            return "msg-\(message.id.uuidString)"
        case .terminal(let terminal):
            return "terminal-\(terminal.id)"
        }
    }
}
