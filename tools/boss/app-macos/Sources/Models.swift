import Foundation

enum ChatRole {
    case user
    case assistant
    case system
}

struct ChatMessage: Identifiable {
    let id = UUID()
    let role: ChatRole
    var text: String
}
