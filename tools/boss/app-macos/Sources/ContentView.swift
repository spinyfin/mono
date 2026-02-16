import AppKit
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
        .task {
            model.startIfNeeded()
        }
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
                    ForEach(model.timeline) { item in
                        switch item {
                        case .message(let message):
                            MessageBubble(message: message)
                                .id(item.id)
                        case .terminal(let terminal):
                            TerminalActivityCard(activity: terminal)
                                .id(item.id)
                        }
                    }
                }
                .padding(16)
            }
            .onReceive(model.$timeline) { _ in
                if let last = model.timeline.last {
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
        switch message.role {
        case .assistant:
            assistantText
        case .user:
            userBubble
        case .system:
            systemText
        }
    }

    private var assistantText: some View {
        HStack {
            Text(message.text)
                .font(.body)
                .textSelection(.enabled)
                .frame(maxWidth: 720, alignment: .leading)
            Spacer(minLength: 60)
        }
    }

    private var userBubble: some View {
        HStack {
            Spacer(minLength: 80)
            Text(message.text)
                .font(.body)
                .textSelection(.enabled)
                .padding(12)
                .frame(maxWidth: 560, alignment: .leading)
                .background(.blue.opacity(0.18))
                .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
        }
    }

    private var systemText: some View {
        HStack {
            Text(message.text)
                .font(.caption)
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
                .frame(maxWidth: 720, alignment: .leading)
            Spacer(minLength: 60)
        }
    }
}

private struct TerminalActivityCard: View {
    let activity: TerminalActivity

    @State private var isExpanded: Bool = false
    @State private var isHovering: Bool = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if isExpanded {
                VStack(spacing: 0) {
                    terminalHeader
                        .padding(.horizontal, 12)
                        .padding(.vertical, 10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(terminalHeaderBackground)

                    Divider()
                        .overlay(Color(nsColor: .separatorColor))

                    TerminalOutputPane(activity: activity, background: terminalOutputBackground)
                }
                .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
                .overlay(
                    RoundedRectangle(cornerRadius: 12, style: .continuous)
                        .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
                )
            } else {
                terminalHeader
                    .padding(.horizontal, 12)
                    .padding(.vertical, 10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(terminalHeaderBackground)
                    .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
                    .overlay(
                        RoundedRectangle(cornerRadius: 12, style: .continuous)
                            .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
                    )
            }
        }
        .onHover { hovering in
            isHovering = hovering
        }
    }

    private var commandPrefix: String {
        if isFailed {
            return "Failed"
        }
        if isSuccessful {
            return "Success"
        }
        return "Running"
    }

    private var command: String {
        let command = activity.command.isEmpty ? "<command unavailable>" : activity.command
        return command
    }

    private var isSuccessful: Bool {
        activity.status == "Done"
    }

    private var isFailed: Bool {
        activity.status.hasPrefix("Failed") || activity.status.hasPrefix("Terminated")
    }

    private var terminalHeader: some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 6) {
                if let cwd = activity.cwd, !cwd.isEmpty {
                    Text(cwd)
                        .font(.system(.footnote, design: .monospaced))
                        .foregroundStyle(.secondary)
                }

                commandLineText
                    .font(.system(.callout, design: .monospaced))
                    .textSelection(.enabled)
            }

            Spacer(minLength: 12)

            Button {
                isExpanded.toggle()
            } label: {
                Image(systemName: isExpanded ? "chevron.up" : "chevron.down")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .frame(width: 22, height: 22)
                    .background(Color(nsColor: .quaternaryLabelColor).opacity(0.22))
                    .clipShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
            }
            .buttonStyle(.plain)
            .help(isExpanded ? "Hide output" : "Show output")
            .opacity(isHovering ? 1 : 0)
            .allowsHitTesting(isHovering)
            .animation(.easeInOut(duration: 0.12), value: isHovering)
        }
    }

    private var statusWordColor: Color {
        if isFailed {
            return .red
        }
        if isSuccessful {
            return .green
        }
        return .primary
    }

    private var commandLineText: Text {
        Text(commandPrefix).foregroundColor(statusWordColor)
            + Text(" \(command)").foregroundColor(.primary)
    }

    private var terminalHeaderBackground: Color {
        Color(nsColor: .controlBackgroundColor)
    }

    private var terminalOutputBackground: Color {
        Color(nsColor: .textBackgroundColor)
    }
}

private struct TerminalOutputPane: View {
    let activity: TerminalActivity
    let background: Color

    @State private var isPinnedToBottom: Bool = true
    @State private var suppressOffsetTracking: Bool = false
    @State private var contentFrame: CGRect = .zero
    @State private var viewportHeight: CGFloat = 0

    private let bottomThreshold: CGFloat = 6

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    Text(activity.output.isEmpty ? "" : activity.output)
                        .font(.system(.callout, design: .monospaced))
                        .frame(maxWidth: .infinity, alignment: .topLeading)
                        .textSelection(.enabled)
                        .padding(12)
                    Color.clear
                        .frame(height: 1)
                        .id(outputBottomID)
                }
                .background(
                    GeometryReader { geo in
                        Color.clear.preference(
                            key: TerminalContentFramePreferenceKey.self,
                            value: geo.frame(in: .named(scrollSpaceID))
                        )
                    }
                )
            }
            .coordinateSpace(name: scrollSpaceID)
            .background(
                GeometryReader { geo in
                    Color.clear.preference(
                        key: TerminalViewportHeightPreferenceKey.self,
                        value: geo.size.height
                    )
                }
            )
            .frame(minHeight: 120, maxHeight: 240)
            .background(background)
            .onPreferenceChange(TerminalContentFramePreferenceKey.self) { frame in
                contentFrame = frame
                refreshPinnedState()
            }
            .onPreferenceChange(TerminalViewportHeightPreferenceKey.self) { height in
                viewportHeight = height
                refreshPinnedState()
            }
            .onAppear {
                scrollToBottom(proxy, animated: false)
                isPinnedToBottom = true
            }
            .onChange(of: activity.output.count) { _, _ in
                guard isPinnedToBottom else {
                    return
                }

                suppressOffsetTracking = true
                scrollToBottom(proxy, animated: true)

                DispatchQueue.main.asyncAfter(deadline: .now() + 0.12) {
                    isPinnedToBottom = true
                    suppressOffsetTracking = false
                }
            }
        }
    }

    private var outputBottomID: String {
        "terminal-output-bottom-\(activity.id)"
    }

    private var scrollSpaceID: String {
        "terminal-scroll-space-\(activity.id)"
    }

    private func scrollToBottom(_ proxy: ScrollViewProxy, animated: Bool) {
        if animated {
            withAnimation(.easeOut(duration: 0.12)) {
                proxy.scrollTo(outputBottomID, anchor: .bottom)
            }
        } else {
            proxy.scrollTo(outputBottomID, anchor: .bottom)
        }
    }

    private func refreshPinnedState() {
        guard !suppressOffsetTracking else {
            return
        }

        let bottomDistance = max(0, contentFrame.height + contentFrame.minY - viewportHeight)
        isPinnedToBottom = bottomDistance <= bottomThreshold
    }
}

private struct TerminalContentFramePreferenceKey: PreferenceKey {
    static let defaultValue: CGRect = .zero

    static func reduce(value: inout CGRect, nextValue: () -> CGRect) {
        value = nextValue()
    }
}

private struct TerminalViewportHeightPreferenceKey: PreferenceKey {
    static let defaultValue: CGFloat = 0

    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = nextValue()
    }
}
