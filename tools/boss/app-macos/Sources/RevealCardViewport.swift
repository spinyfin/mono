import AppKit
import SwiftUI

/// Measures the board viewport, outside its horizontally scrolling content.
struct RevealBoardViewport: NSViewRepresentable {
    let model: ChatViewModel

    func makeNSView(context: Context) -> NSView { NSView() }
    func updateNSView(_ view: NSView, context: Context) {
        model.revealBoardViewport = view
    }
}

/// A card being instantiated is not proof that it is visible: lazy stacks
/// prefetch off-screen rows. Check AppKit's clipped visible rectangle after
/// layout, including both the column and the horizontal board scroll views.
struct RevealCardViewport: NSViewRepresentable {
    let cardID: String
    let generation: UUID
    let model: ChatViewModel

    func makeNSView(context: Context) -> NSView { NSView() }

    func updateNSView(_ view: NSView, context: Context) {
        if BossRevealCapture.viewportTargetID == cardID {
            BossRevealCapture.targetViewport = view
        }
        guard context.coordinator.generation != generation else { return }
        context.coordinator.generation = generation
        context.coordinator.poll?.cancel()
        context.coordinator.poll = Task { @MainActor [weak view, weak model] in
            for attempt in 0..<50 {
                if attempt > 0 {
                    do { try await Task.sleep(for: .milliseconds(50)) } catch { return }
                }
                guard let view, let model,
                      model.revealGeneration == generation,
                      model.revealScrollTarget == cardID else { return }
                guard Self.isVisible(view: view, board: model.revealBoardViewport) else { continue }
                model.confirmReveal(cardID: cardID, generation: generation)
                return
            }
        }
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    /// Read live geometry, rather than a cached reveal-time visibility result.
    static func isVisible(view: NSView, board: NSView?) -> Bool {
        guard let window = view.window, !window.isMiniaturized,
              window.isVisible || BossCaptureArgs.shared.isCaptureMode,
              !view.isHiddenOrHasHiddenAncestor,
              view.bounds.width > 1, view.bounds.height > 1,
              let board, board.window === window else { return false }
        let card = view.convert(view.bounds, to: nil)
        let visible = view.convert(view.visibleRect, to: nil)
            .intersection(board.convert(board.visibleRect, to: nil))
        // Oversized cards must have their center and a substantial strip visible.
        return isVisible(card: card, clippedTo: visible)
    }

    static func isVisible(card: NSRect, clippedTo viewport: NSRect) -> Bool {
        let visible = card.intersection(viewport)
        return visible.width >= min(card.width, 100)
            && visible.height >= min(card.height, 60)
            && visible.contains(NSPoint(x: card.midX, y: card.midY))
    }

    static func dismantleNSView(_ view: NSView, coordinator: Coordinator) {
        coordinator.poll?.cancel()
    }

    final class Coordinator {
        var generation: UUID?
        var poll: Task<Void, Never>?
    }
}
