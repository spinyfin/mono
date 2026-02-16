import Foundation

@MainActor
final class ChatViewModel: ObservableObject {
    @Published var messages: [ChatMessage] = [
        ChatMessage(
            role: .system,
            text: "Boss PoC frontend scaffold. Engine integration comes in the next commit."
        ),
    ]
    @Published var draft: String = ""

    func sendDraft() {
        let trimmed = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            return
        }

        messages.append(ChatMessage(role: .user, text: trimmed))
        messages.append(
            ChatMessage(
                role: .assistant,
                text: "(placeholder) engine is not connected in this commit"
            )
        )
        draft = ""
    }
}
