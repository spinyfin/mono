import AppKit
import SwiftUI
import Textual

struct ContentView: View {
    @StateObject private var model = ChatViewModel()

    var body: some View {
        NavigationSplitView {
            sidebar
        } detail: {
            detail
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

    private var sidebar: some View {
        List(model.agents, selection: $model.selectedAgentID) { agent in
            HStack {
                Image(systemName: "person.circle")
                    .foregroundStyle(.secondary)
                VStack(alignment: .leading, spacing: 2) {
                    Text(agent.name)
                        .font(.body)
                    if !agent.isReady {
                        Text("Starting…")
                            .font(.caption)
                            .foregroundStyle(.orange)
                    } else if agent.isSending {
                        Text("Working…")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .tag(agent.id)
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom) {
            HStack {
                Button {
                    model.createAgent()
                } label: {
                    Label("New Agent", systemImage: "plus")
                }
                .buttonStyle(.borderless)
                Spacer()
                if !model.isConnected {
                    Label("Disconnected", systemImage: "circle.fill")
                        .foregroundStyle(.red)
                        .font(.caption)
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
        .navigationSplitViewColumnWidth(min: 180, ideal: 220, max: 320)
    }

    private var detail: some View {
        VStack(spacing: 0) {
            messageList
            composer
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private var messageList: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    ForEach(model.selectedAgentTimeline) { item in
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
            .onChange(of: model.selectedAgentTimeline.count) {
                if let last = model.selectedAgentTimeline.last {
                    DispatchQueue.main.async {
                        proxy.scrollTo(last.id, anchor: .bottom)
                    }
                }
            }
        }
    }

    private var composer: some View {
        let isDraftEmpty = model.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        let canSend = model.selectedAgentID != nil && !isDraftEmpty && !model.isSelectedAgentSending && model.isSelectedAgentReady

        return VStack(spacing: 0) {
            HStack(alignment: .center, spacing: 10) {
                ComposerTextView(text: $model.draft, placeholder: model.isSelectedAgentReady ? "Type a message…" : "Agent starting…", autoFocus: true, focusTrigger: model.selectedAgentID) {
                    model.sendDraft()
                }
                .frame(height: 36)
                .frame(maxWidth: .infinity)

                Button {
                    model.sendDraft()
                } label: {
                    Image(systemName: "paperplane.fill")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(canSend ? .primary : .secondary)
                        .frame(width: 18, height: 18)
                }
                .buttonStyle(.plain)
                .keyboardShortcut(.return, modifiers: [.command])
                .disabled(!canSend)
                .help("Send")
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .background(Color(nsColor: .controlBackgroundColor))
            .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
            )
            .padding(.horizontal, 16)
            .padding(.bottom, 12)
            .padding(.top, 4)

            if model.isSelectedAgentSending {
                HStack(spacing: 6) {
                    ProgressView()
                        .controlSize(.mini)
                    Text("Working…")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer()
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
            }
        }
    }
}

private struct ComposerTextView: NSViewRepresentable {
    @Binding var text: String
    let placeholder: String
    let autoFocus: Bool
    var focusTrigger: String?
    let onSubmit: () -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(parent: self)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = NSScrollView()
        scrollView.drawsBackground = false
        scrollView.borderType = .noBorder
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.scrollerStyle = .overlay

        let textView = ComposerNSTextView()
        textView.delegate = context.coordinator
        textView.isEditable = true
        textView.isSelectable = true
        textView.isRichText = false
        textView.importsGraphics = false
        textView.allowsUndo = true
        textView.font = .preferredFont(forTextStyle: .body)
        textView.textColor = .labelColor
        textView.backgroundColor = .clear
        textView.drawsBackground = false
        textView.focusRingType = .none
        textView.textContainer?.lineFragmentPadding = 0
        textView.isHorizontallyResizable = false
        textView.isVerticallyResizable = true
        textView.autoresizingMask = [.width]
        textView.maxSize = NSSize(
            width: CGFloat.greatestFiniteMagnitude,
            height: CGFloat.greatestFiniteMagnitude
        )
        textView.minSize = NSSize(width: 0, height: 0)
        textView.textContainer?.widthTracksTextView = true
        textView.submitHandler = onSubmit
        textView.placeholder = placeholder
        textView.string = text

        scrollView.documentView = textView
        context.coordinator.textView = textView
        context.coordinator.didAutoFocus = false
        return scrollView
    }

    func updateNSView(_ nsView: NSScrollView, context: Context) {
        context.coordinator.parent = self
        guard let textView = context.coordinator.textView else {
            return
        }

        textView.submitHandler = onSubmit
        textView.placeholder = placeholder
        if textView.string != text {
            textView.string = text
            textView.needsDisplay = true
        }

        let shouldFocus: Bool
        if !context.coordinator.didAutoFocus, autoFocus {
            context.coordinator.didAutoFocus = true
            shouldFocus = true
        } else if focusTrigger != context.coordinator.lastFocusTrigger {
            context.coordinator.lastFocusTrigger = focusTrigger
            shouldFocus = true
        } else {
            shouldFocus = false
        }

        if shouldFocus {
            DispatchQueue.main.async {
                guard let window = textView.window else {
                    return
                }
                window.makeFirstResponder(textView)
            }
        }
    }

    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: ComposerTextView
        weak var textView: ComposerNSTextView?
        var didAutoFocus = false
        var lastFocusTrigger: String?

        init(parent: ComposerTextView) {
            self.parent = parent
        }

        func textDidChange(_ notification: Notification) {
            guard let textView = notification.object as? NSTextView else {
                return
            }
            parent.text = textView.string
            textView.needsDisplay = true
        }
    }
}

private final class ComposerNSTextView: NSTextView {
    var submitHandler: (() -> Void)?
    var placeholder: String = "" {
        didSet {
            needsDisplay = true
        }
    }

    override func layout() {
        super.layout()
        guard let layoutManager, let textContainer, let scrollView = enclosingScrollView else { return }
        layoutManager.ensureLayout(for: textContainer)
        let textHeight = layoutManager.usedRect(for: textContainer).height
        let visibleHeight = scrollView.contentSize.height
        let topInset = max(0, (visibleHeight - textHeight) / 2)
        if abs(textContainerInset.height - topInset) > 0.5 {
            textContainerInset = NSSize(width: 0, height: topInset)
        }
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)

        guard string.isEmpty, !placeholder.isEmpty, let font else {
            return
        }

        let origin = textContainerOrigin
        let x = origin.x + (textContainer?.lineFragmentPadding ?? 0)
        let y = origin.y
        let attrs: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: NSColor.placeholderTextColor,
        ]
        (placeholder as NSString).draw(at: NSPoint(x: x, y: y), withAttributes: attrs)
    }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        guard event.type == .keyDown else {
            return super.performKeyEquivalent(with: event)
        }

        let modifiers = event.modifierFlags.intersection([.command, .shift, .option, .control])
        guard modifiers == [.command], let chars = event.charactersIgnoringModifiers else {
            return super.performKeyEquivalent(with: event)
        }

        switch chars.lowercased() {
        case "a":
            selectAll(nil)
            return true
        case "c":
            copy(nil)
            return true
        case "v":
            paste(nil)
            return true
        case "x":
            cut(nil)
            return true
        case "z":
            undoManager?.undo()
            return true
        default:
            return super.performKeyEquivalent(with: event)
        }
    }

    override func doCommand(by selector: Selector) {
        let isNewlineCommand = selector == #selector(insertNewline(_:))
            || selector == #selector(insertLineBreak(_:))
            || selector == #selector(insertNewlineIgnoringFieldEditor(_:))
        guard isNewlineCommand, !hasMarkedText() else {
            super.doCommand(by: selector)
            return
        }

        let modifiers = NSApp.currentEvent?.modifierFlags.intersection([
            .shift,
            .control,
            .option,
            .command,
        ]) ?? []

        if modifiers == [.shift] {
            insertNewline(nil)
            return
        }

        if modifiers.isEmpty {
            submitHandler?()
            return
        }

        super.doCommand(by: selector)
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
            StructuredText(markdown: message.text)
                .textual.textSelection(.enabled)
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
