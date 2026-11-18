import AppKit
import OSLog
import SwiftUI

/// Reveal is synchronous; responder focus waits for SwiftUI to mount the page.
@MainActor
final class AgentFocusCoordinator {
    private var pending: Task<Void, Never>?
    private let cards = NSMapTable<NSString, NSView>(keyOptions: .strongMemory, valueOptions: .weakMemory)
    var report: (String, EngineFocusResult) -> Void = { runId, result in
        let logger = Logger(subsystem: "com.boss.app", category: "AgentFocus")
        switch result {
        case .success:
            logger.info("Focused agent \(runId, privacy: .public)")
        case .failure(let error):
            logger.error("Agent focus failed for \(runId, privacy: .public): \(String(describing: error), privacy: .public)")
        }
    }

    /// How `request` waits between two attempts. Tests replace it, so that
    /// exhausting the retry budget takes them no real time.
    var retrySleep: @MainActor (Duration) async throws -> Void = { try await Task.sleep(for: $0) }

    func register(_ view: NSView, runId: String) {
        cards.setObject(view, forKey: runId as NSString)
    }

    func focusCard(runId: String) -> EngineFocusResult {
        guard let view = cards.object(forKey: runId as NSString), let window = view.window else {
            return .failure(.internalFailure("agent card has no host window"))
        }
        NSApp.activate(ignoringOtherApps: true)
        if window.isMiniaturized { window.deminiaturize(nil) }
        window.makeKeyAndOrderFront(nil)
        guard window.makeFirstResponder(view) else {
            return .failure(.internalFailure("agent card refused keyboard focus"))
        }
        return .success
    }

    func request(runId: String, attempt: @escaping @MainActor () -> EngineFocusResult) {
        pending?.cancel()
        let sleep = retrySleep
        pending = Task { @MainActor [weak self] in
            // Retry readiness for up to one second, rather than assuming a
            // fixed rendering delay. A detached terminal/card reports failure.
            for index in 0..<21 {
                guard !Task.isCancelled else { return }
                let result = attempt()
                if case .success = result {
                    self?.report(runId, result)
                    return
                }
                if index == 20 {
                    self?.report(runId, result)
                    return
                }
                do { try await sleep(.milliseconds(50)) }
                catch { return }
            }
        }
    }
}

struct AgentCardFocusTarget: NSViewRepresentable {
    let coordinator: AgentFocusCoordinator
    let runId: String

    func makeNSView(context: Context) -> NSView {
        let view = AgentCardResponder()
        coordinator.register(view, runId: runId)
        return view
    }

    func updateNSView(_ view: NSView, context: Context) {
        coordinator.register(view, runId: runId)
    }
}

private final class AgentCardResponder: NSView {
    override var acceptsFirstResponder: Bool { true }
}
