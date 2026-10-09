import AppKit

/// Only redacted fields survive observation; event and window are weak
/// identity tokens, never retained. A fallback must match the active dispatch.
@MainActor
final class TerminalInputKeyContext {
    private weak var event: NSEvent?
    private weak var window: NSWindow?
    private var redacted: [String: Any] = [:]

    func record(_ event: NSEvent, window: NSWindow) {
        self.event = event
        self.window = window
        redacted = TerminalInputDescribe.keyFields(
            keyCode: event.keyCode, characters: event.charactersIgnoringModifiers,
            modifierFlags: event.modifierFlags
        )
    }

    func fields(for current: NSEvent?, window: NSWindow, selector: Selector) -> [String: Any] {
        guard TerminalInputFallback.isBeepCandidate(selector),
              let current, current.type == .keyDown, current === event,
              self.window === window, current.windowNumber == window.windowNumber else { return [:] }
        return redacted
    }
}

@MainActor
enum TerminalInputFallback {
    static func isBeepCandidate(_ selector: Selector) -> Bool {
        selector == #selector(NSResponder.keyDown(with:))
    }

    static func window(for responder: NSResponder) -> NSWindow? {
        if let window = responder as? NSWindow { return window }
        if let controller = responder as? NSWindowController {
            return controller.isWindowLoaded ? controller.window : nil
        }
        if let controller = responder as? NSViewController {
            return controller.viewIfLoaded?.window
        }
        return (responder as? NSView)?.window
    }

    static func tail(of responder: NSResponder) -> NSResponder {
        var tail = responder
        var visited: Set<ObjectIdentifier> = [ObjectIdentifier(tail)]
        while let next = tail.nextResponder, visited.insert(ObjectIdentifier(next)).inserted {
            tail = next
        }
        return tail
    }
}
