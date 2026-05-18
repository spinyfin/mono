@preconcurrency import AppKit
import OSLog
import SwiftUI

/// Owns the in-memory comment array for a single markdown viewer instance
/// and coordinates the selection → authoring → sidebar → highlight flow.
///
/// Phase 1: all state is in-memory; no engine RPCs; closing the viewer
/// loses all comments. This is intentional and surfaced to the user in
/// the sidebar header.
@MainActor
final class CommentLayer: NSObject, ObservableObject {
    @Published var comments: [Comment] = []
    @Published var isShowingPopover: Bool = false
    @Published var pendingQuotedText: String = ""
    /// Character that seeded the form via type-to-comment entry path.
    @Published var pendingFirstChar: Character? = nil
    /// Quoted text of the comment just clicked in the sidebar; clears after the flash.
    @Published var flashingText: String? = nil

    // NSEvent monitor tokens; stored nonisolated(unsafe) because the opaque Any
    // tokens are installed/removed only on the main actor.
    nonisolated(unsafe) private var keyMonitor: Any?
    nonisolated(unsafe) private var rightClickMonitor: Any?
    // mouseUpMonitor tracks left-mouse-up in Textual's NSTextInteractionView so we can
    // anchor the popover to the text selection even though NSTextInteractionView is not
    // an NSTextView and never posts NSTextView.didChangeSelectionNotification.
    nonisolated(unsafe) private var mouseUpMonitor: Any?

    /// The NSTextView whose selection seeded the pending comment request.
    /// Captured from NSTextView.didChangeSelectionNotification (the object is the text view).
    /// Queried at present-time via firstRect(forCharacterRange:) — never cached as screen coords.
    private weak var anchorTextView: NSTextView?

    /// Anchor for Textual's NSTextInteractionView (an NSView, NOT NSTextView).
    /// Textual's selection layer does not use NSTextView, so NSTextView.didChangeSelectionNotification
    /// never fires for design-doc text selections. Instead we track leftMouseUp events: when the
    /// user releases the mouse over an NSTextInteractionView (Textual's selection handler),
    /// we store the view-local coordinates of the mouse release so we can position the popover
    /// relative to the selection even if the user scrolls between selection and click.
    private weak var anchorTextInteractionView: NSView?
    private var anchorLocalPoint: NSPoint?

    /// The live NSPopover, if one is currently visible.
    private var activePopover: NSPopover?

    // Logger for anchor diagnostics. Leave at .info so future regressions are diagnosable.
    private static let logger = Logger(subsystem: "com.boss.app", category: "CommentPopupAnchor")

    // MARK: - Monitor lifecycle

    func installMonitors() {
        // ObjC selector form avoids the @Sendable closure constraint on the block-based
        // addObserver API, which would make `notification` a sending parameter and prevent
        // capturing the non-Sendable NSTextView inside assumeIsolated.
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(textViewSelectionDidChange(_:)),
            name: NSTextView.didChangeSelectionNotification,
            object: nil
        )
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self else { return event }
            let chars = event.charactersIgnoringModifiers
            let mods = event.modifierFlags
            let consume = MainActor.assumeIsolated {
                self.shouldConsumeKeyEvent(chars: chars, mods: mods)
            }
            return consume ? nil : event
        }
        rightClickMonitor = NSEvent.addLocalMonitorForEvents(matching: .rightMouseDown) { [weak self] event in
            guard let self else { return event }
            let loc = event.locationInWindow
            let win = event.window
            let consume = MainActor.assumeIsolated {
                self.handleRightClick(locationInWindow: loc, window: win)
            }
            return consume ? nil : event
        }
        mouseUpMonitor = NSEvent.addLocalMonitorForEvents(matching: .leftMouseUp) { [weak self] event in
            guard let self else { return event }
            let win = event.window
            let loc = event.locationInWindow
            MainActor.assumeIsolated {
                self.captureTextInteractionAnchor(window: win, locInWindow: loc)
            }
            return event
        }
    }

    func removeMonitors() {
        if let m = keyMonitor { NSEvent.removeMonitor(m); keyMonitor = nil }
        if let m = rightClickMonitor { NSEvent.removeMonitor(m); rightClickMonitor = nil }
        if let m = mouseUpMonitor { NSEvent.removeMonitor(m); mouseUpMonitor = nil }
        NotificationCenter.default.removeObserver(
            self, name: NSTextView.didChangeSelectionNotification, object: nil)
        activePopover?.close()
        anchorTextInteractionView = nil
        anchorLocalPoint = nil
    }

    /// Called by NotificationCenter on the main thread when any NSTextView changes selection.
    /// Using the ObjC selector form avoids @Sendable parameter constraints that prevent
    /// capturing the non-Sendable NSTextView across a @Sendable closure boundary.
    @objc nonisolated private func textViewSelectionDidChange(_ notification: Notification) {
        let textView = notification.object as? NSTextView
        MainActor.assumeIsolated { [weak self] in
            guard let self, !self.isShowingPopover else { return }
            // Only update while the popover is closed; the comment form's own
            // NSTextView (CommentTextEditor) would otherwise overwrite the anchor.
            self.anchorTextView = textView
            // An NSTextView gained focus, so clear any Textual interaction view anchor —
            // the user is now selecting in a different text layer.
            self.anchorTextInteractionView = nil
            self.anchorLocalPoint = nil
            let tvDesc = String(describing: textView)
            Self.logger.info("CommentPopupAnchor: (a) NSTextView selection-change; anchorTextView=\(tvDesc)")
        }
    }

    // MARK: - Authoring

    func requestNewComment(firstChar: Character? = nil) {
        pendingQuotedText = captureCurrentSelection() ?? ""
        pendingFirstChar = firstChar

        guard let (posRect, posView) = resolveAnchor() else { return }

        let posRectStr = NSStringFromRect(posRect)
        let posViewDesc = String(describing: posView)
        let posWindowDesc = String(describing: posView.window)
        Self.logger.info("CommentPopupAnchor: (b) Add Comment clicked; posRect=\(posRectStr) posView=\(posViewDesc) posView.window=\(posWindowDesc)")

        let popover = NSPopover()
        popover.contentViewController = NSHostingController(
            rootView: CommentPopover(layer: self)
        )
        // Transient: clicks outside the popover dismiss it automatically, matching
        // the previous SwiftUI .popover default behaviour.
        popover.behavior = .transient
        // NSPopover.delegate is weak; self outlives the popover so this is safe.
        popover.delegate = self
        activePopover = popover
        isShowingPopover = true

        popover.show(relativeTo: posRect, of: posView, preferredEdge: .maxY)
    }

    func addComment(quoted: String, body: String) {
        guard !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        let comment = Comment(
            id: UUID(),
            quotedText: quoted,
            body: body.trimmingCharacters(in: .whitespacesAndNewlines),
            createdAt: Date()
        )
        comments.append(comment)
        activePopover?.close()
        activePopover = nil
        isShowingPopover = false
        pendingQuotedText = ""
        pendingFirstChar = nil
    }

    func cancelNewComment() {
        activePopover?.close()
    }

    func dismiss(_ comment: Comment) {
        comments.removeAll { $0.id == comment.id }
    }

    // MARK: - Click-to-jump

    func jumpTo(_ comment: Comment) {
        let text = comment.quotedText
        flashingText = text
        Task {
            try? await Task.sleep(for: .milliseconds(900))
            if flashingText == text { flashingText = nil }
        }
    }

    // MARK: - Selection helpers

    /// Non-destructively checks whether the current first responder has a text selection
    /// by asking it to validate the "Copy" UI item. Textual's NSTextInteractionView
    /// implements NSUserInterfaceValidations: validateUserInterfaceItem returns true for
    /// "copy" only when there is a non-empty selection.
    func hasCurrentSelection() -> Bool {
        guard let firstResponder = NSApp.keyWindow?.firstResponder else { return false }
        let copyItem = NSMenuItem(
            title: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        var responder: NSResponder? = firstResponder
        while let current = responder {
            if let validator = current as? NSUserInterfaceValidations {
                return validator.validateUserInterfaceItem(copyItem)
            }
            responder = current.nextResponder
        }
        return false
    }

    /// Returns the positioning rect (in positioningView coords) and view for NSPopover.
    /// Three paths, tried in order:
    ///   1. NSTextView (standard AppKit text): query firstRect(forCharacterRange:) live.
    ///   2. NSTextInteractionView (Textual): use the view-local point captured at mouseUp time.
    ///      This view does NOT subclass NSTextView and never posts
    ///      NSTextView.didChangeSelectionNotification, so we track it separately via a mouseUp
    ///      event monitor (see captureTextInteractionAnchor). View-local coords survive scrolling
    ///      between selection and clicking "Add Comment".
    ///   3. Fallback: near the top-centre of the key window content view.
    private func resolveAnchor() -> (NSRect, NSView)? {
        // Path 1: Standard NSTextView
        if let tv = anchorTextView, let window = tv.window {
            let range = tv.selectedRange()
            if range.length > 0, range.location != NSNotFound {
                let lastCharRange = NSRange(location: range.upperBound - 1, length: 1)
                var actualRange = NSRange()
                let screenRect = tv.firstRect(forCharacterRange: lastCharRange, actualRange: &actualRange)
                if screenRect != .zero {
                    // screen → window → text view coordinates; AppKit handles the conversion
                    // correctly for any display arrangement without explicit screen lookup.
                    let windowRect = window.convertFromScreen(screenRect)
                    let viewRect = tv.convert(windowRect, from: nil)
                    let srStr = NSStringFromRect(screenRect)
                    let vrStr = NSStringFromRect(viewRect)
                    Self.logger.info("CommentPopupAnchor: resolveAnchor via NSTextView; screenRect=\(srStr) viewRect=\(vrStr)")
                    return (viewRect, tv)
                }
            }
        }

        // Path 2: Textual's NSTextInteractionView (captured at leftMouseUp time)
        if let view = anchorTextInteractionView,
           let window = view.window,
           window == NSApp.keyWindow,
           let localPoint = anchorLocalPoint {
            let anchorRect = NSRect(
                x: localPoint.x - 4, y: localPoint.y - 4, width: 8, height: 8)
            let lpStr = NSStringFromPoint(localPoint)
            let arStr = NSStringFromRect(anchorRect)
            Self.logger.info("CommentPopupAnchor: resolveAnchor via NSTextInteractionView; localPoint=\(lpStr) anchorRect=\(arStr)")
            return (anchorRect, view)
        }

        // Fallback: anchor near the top-centre of the key window.
        // NSHostingView (the window's contentView) is flipped (isFlipped=true), so y=0 is at
        // the visual top. Using y=60 positions the anchor 60pt below the top regardless.
        if let contentView = NSApp.keyWindow?.contentView {
            let yPos: CGFloat = contentView.isFlipped
                ? 60
                : contentView.bounds.maxY - 60
            let fallback = NSRect(x: contentView.bounds.midX - 8, y: yPos, width: 16, height: 16)
            let atvDesc = String(describing: anchorTextView)
            let ativDesc = String(describing: anchorTextInteractionView)
            let alpDesc = String(describing: anchorLocalPoint)
            let fbStr = NSStringFromRect(fallback)
            let flipped = contentView.isFlipped
            Self.logger.warning("CommentPopupAnchor: resolveAnchor using fallback; anchorTextView=\(atvDesc) anchorTextInteractionView=\(ativDesc) anchorLocalPoint=\(alpDesc) fallback=\(fbStr) isFlipped=\(flipped)")
            return (fallback, contentView)
        }
        return nil
    }

    /// Called from the leftMouseUp NSEvent monitor. If the event's first responder is
    /// Textual's NSTextInteractionView, records the mouse-release point in the view's own
    /// coordinate space. View-local coordinates remain valid after scrolling — AppKit converts
    /// them to screen space at NSPopover presentation time.
    ///
    /// NSTextInteractionView (Textual's selection overlay) calls window.makeFirstResponder(self)
    /// in mouseDown, so by mouseUp it IS the first responder whenever text is being selected.
    /// The class name check guards against false positives from unrelated NSViews.
    private func captureTextInteractionAnchor(window: NSWindow?, locInWindow: NSPoint) {
        guard !isShowingPopover, let window else { return }
        guard let firstResponder = window.firstResponder as? NSView else { return }

        // Textual's NSTextInteractionView is NOT an NSTextView; it's an NSView subclass
        // internal to the Textual package. We identify it by class name because we cannot
        // import the internal type. If this class name ever changes, the fallback anchor
        // is used instead (no worse than before this fix).
        let typeName = String(describing: type(of: firstResponder))
        guard typeName == "NSTextInteractionView" else { return }

        // Convert the mouse-release point to the view's own coordinate space.
        // NSTextInteractionView has isFlipped=true; AppKit handles the Y-flip when converting
        // from window coordinates (Y-up) to the view's space (Y-down).
        let localPoint = firstResponder.convert(locInWindow, from: nil)
        anchorTextInteractionView = firstResponder
        anchorLocalPoint = localPoint

        let liwStr = NSStringFromPoint(locInWindow)
        let lpStr2 = NSStringFromPoint(localPoint)
        Self.logger.info("CommentPopupAnchor: (a) NSTextInteractionView selection-change; typeName=\(typeName) locInWindow=\(liwStr) localPoint=\(lpStr2)")
    }

    /// Reads the selection via pasteboard copy. Acceptable Phase 1 trade-off:
    /// called only when the user explicitly opens the comment form.
    private func captureCurrentSelection() -> String? {
        let before = NSPasteboard.general.changeCount
        NSApp.sendAction(#selector(NSText.copy(_:)), to: nil, from: nil)
        guard NSPasteboard.general.changeCount != before else { return nil }
        return NSPasteboard.general.string(forType: .string)
    }

    // MARK: - Event handling (called from monitor closures via MainActor.assumeIsolated)

    /// Returns true if the key event should be consumed (opens the comment form).
    private func shouldConsumeKeyEvent(
        chars: String?,
        mods: NSEvent.ModifierFlags
    ) -> Bool {
        guard !isShowingPopover else { return false }
        let cleanMods = mods.intersection(.deviceIndependentFlagsMask)
        guard cleanMods.isSubset(of: [.shift, .capsLock]) else { return false }
        guard
            let str = chars,
            str.count == 1,
            let char = str.first,
            char.isLetter || char.isNumber || char.isPunctuation || char.isSymbol
        else { return false }
        guard hasCurrentSelection() else { return false }
        requestNewComment(firstChar: char)
        return true
    }

    /// Returns true if the right-click event is consumed (shows our custom context menu).
    private func handleRightClick(locationInWindow: NSPoint, window: NSWindow?) -> Bool {
        guard !isShowingPopover, hasCurrentSelection() else { return false }
        guard let window, let view = window.contentView else { return false }

        let menu = NSMenu()
        let target = CommentMenuTarget(layer: self)
        let addItem = NSMenuItem(
            title: "Add Comment",
            action: #selector(CommentMenuTarget.addCommentAction),
            keyEquivalent: ""
        )
        addItem.target = target
        addItem.representedObject = target   // keep target alive during menu
        menu.addItem(addItem)
        menu.addItem(.separator())
        menu.addItem(
            NSMenuItem(title: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c"))

        menu.popUp(positioning: nil, at: locationInWindow, in: view)
        return true
    }
}

// MARK: - NSPopoverDelegate

extension CommentLayer: NSPopoverDelegate {
    /// Called by AppKit when the popover finishes closing, whether by user dismissal or
    /// programmatic close. Resets authoring state. The extension lives in the same file
    /// so it can access private members directly.
    nonisolated func popoverDidClose(_ notification: Notification) {
        Task { @MainActor [weak self] in
            guard let self else { return }
            self.isShowingPopover = false
            self.pendingFirstChar = nil
            self.pendingQuotedText = ""
            self.activePopover = nil
        }
    }
}

// MARK: - Menu action target

private final class CommentMenuTarget: NSObject, @unchecked Sendable {
    let layer: CommentLayer
    init(layer: CommentLayer) { self.layer = layer }

    @objc func addCommentAction(_ sender: Any?) {
        Task { @MainActor in layer.requestNewComment() }
    }
}

// MARK: - View modifier

/// Wraps a markdown viewer with the full comment affordance:
/// sidebar (when comments exist), "Add Comment" button, popover authoring form,
/// and three entry paths (type-to-comment, right-click context menu, ⌘⇧K).
///
/// Usage:
/// ```swift
/// MarkdownViewerView(...)
///     .withComments()
/// ```
struct WithCommentsModifier: ViewModifier {
    @StateObject private var layer = CommentLayer()

    func body(content: Content) -> some View {
        let commentedTexts = layer.comments.map(\.quotedText).filter { !$0.isEmpty }
        let flashingText = layer.flashingText

        HStack(spacing: 0) {
            content
                .environment(\.commentedTexts, commentedTexts)
                .environment(\.commentFlashText, flashingText)

            if !layer.comments.isEmpty {
                Divider()
                CommentSidebar(layer: layer)
                    .frame(width: 280)
            }
        }
        .overlay(alignment: .topTrailing) {
            if layer.comments.isEmpty {
                addCommentButton
                    .padding(.trailing, 16)
                    .padding(.top, 20)
            }
        }
        // Hidden button for ⌘⇧K shortcut (⌘⇧M is already the Metrics panel shortcut).
        .background {
            Button("") {
                if layer.hasCurrentSelection() { layer.requestNewComment() }
            }
            .keyboardShortcut("k", modifiers: [.command, .shift])
            .frame(width: 0, height: 0)
            .hidden()
        }
        .onAppear { layer.installMonitors() }
        .onDisappear { layer.removeMonitors() }
    }

    private var addCommentButton: some View {
        Button {
            layer.requestNewComment()
        } label: {
            Label("Add Comment", systemImage: "bubble.left.and.text.bubble.right")
                .font(.callout)
        }
        .buttonStyle(.bordered)
        .controlSize(.small)
        .help("Select text, then click or press ⌘⇧K to add a comment")
    }
}

extension View {
    func withComments() -> some View {
        modifier(WithCommentsModifier())
    }
}

// MARK: - Environment keys

private struct CommentedTextsKey: EnvironmentKey {
    static var defaultValue: [String] { [] }
}

private struct CommentFlashTextKey: EnvironmentKey {
    static var defaultValue: String? { nil }
}

extension EnvironmentValues {
    var commentedTexts: [String] {
        get { self[CommentedTextsKey.self] }
        set { self[CommentedTextsKey.self] = newValue }
    }

    var commentFlashText: String? {
        get { self[CommentFlashTextKey.self] }
        set { self[CommentFlashTextKey.self] = newValue }
    }
}
