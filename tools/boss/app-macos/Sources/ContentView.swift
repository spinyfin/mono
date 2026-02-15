import SwiftUI

struct ContentView: View {
    @StateObject private var model = ChatViewModel()

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            messageList
            Divider()
            composer
        }
        .frame(minWidth: 860, minHeight: 560)
        .alert(item: $model.pendingPermission) { request in
            Alert(
                title: Text("Permission Request"),
                message: Text(request.title),
                primaryButton: .default(Text("Allow")) {
                    model.respondToPendingPermission(granted: true)
                },
                secondaryButton: .destructive(Text("Deny")) {
                    model.respondToPendingPermission(granted: false)
                }
            )
        }
    }

    private var header: some View {
        HStack {
            Text("Boss")
                .font(.title2.weight(.semibold))
            Spacer()
            Label(model.isConnected ? "Connected" : "Disconnected", systemImage: "circle.fill")
                .foregroundStyle(model.isConnected ? .green : .red)
                .font(.callout)
            if model.isSending {
                ProgressView()
                    .controlSize(.small)
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
        .background(.regularMaterial)
    }

    private var messageList: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    ForEach(model.messages) { message in
                        MessageBubble(message: message)
                            .id(message.id)
                    }
                }
                .padding(16)
            }
            .onChange(of: model.messages.count) { _, _ in
                if let last = model.messages.last {
                    proxy.scrollTo(last.id, anchor: .bottom)
                }
            }
        }
    }

    private var composer: some View {
        HStack(alignment: .bottom, spacing: 12) {
            TextField("Type a message…", text: $model.draft, axis: .vertical)
                .lineLimit(4)
                .textFieldStyle(.roundedBorder)
                .onSubmit {
                    model.sendDraft()
                }

            Button("Send") {
                model.sendDraft()
            }
            .keyboardShortcut(.return, modifiers: [.command])
            .buttonStyle(.borderedProminent)
            .disabled(model.isSending)
        }
        .padding(16)
    }
}

private struct MessageBubble: View {
    let message: ChatMessage

    var body: some View {
        HStack {
            if message.role == .user {
                Spacer(minLength: 60)
            }

            VStack(alignment: .leading, spacing: 6) {
                Text(label)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Text(message.text)
                    .textSelection(.enabled)
            }
            .padding(12)
            .frame(maxWidth: 560, alignment: .leading)
            .background(background)
            .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))

            if message.role != .user {
                Spacer(minLength: 60)
            }
        }
    }

    private var label: String {
        switch message.role {
        case .user:
            return "You"
        case .assistant:
            return "Agent"
        case .system:
            return "System"
        }
    }

    private var background: some ShapeStyle {
        switch message.role {
        case .user:
            return AnyShapeStyle(.blue.opacity(0.18))
        case .assistant:
            return AnyShapeStyle(.gray.opacity(0.16))
        case .system:
            return AnyShapeStyle(.orange.opacity(0.16))
        }
    }
}
