import AppKit
import SwiftUI

/// A card being instantiated is not proof that it is visible: lazy stacks
/// prefetch off-screen rows. Check AppKit's clipped visible rectangle after
/// layout, including both the column and the horizontal board scroll views.
struct RevealCardViewport: NSViewRepresentable {
    let cardID: String
    let generation: UUID
    let model: ChatViewModel

    func makeNSView(context: Context) -> NSView { NSView() }

    func updateNSView(_ view: NSView, context: Context) {
        context.coordinator.poll?.cancel()
        context.coordinator.poll = Task { @MainActor [weak view, weak model] in
            for _ in 0..<50 {
                do { try await Task.sleep(for: .milliseconds(50)) } catch { return }
                guard let view, let model,
                      model.revealGeneration == generation,
                      model.revealScrollTarget == cardID else { return }
                guard let window = view.window, !window.isMiniaturized,
                      window.isVisible || BossCaptureArgs.shared.isCaptureMode,
                      !view.isHiddenOrHasHiddenAncestor,
                      view.bounds.width > 1, view.bounds.height > 1 else { continue }
                let visible = view.visibleRect
                // A card can be taller than the viewport. Its center and a
                // substantial strip must be visible; a clipped sliver is not
                // enough to claim the card was revealed.
                guard visible.width >= min(view.bounds.width, 100),
                      visible.height >= min(view.bounds.height, 60),
                      visible.contains(NSPoint(x: view.bounds.midX, y: view.bounds.midY))
                else { continue }
                model.confirmReveal(cardID: cardID, generation: generation)
                return
            }
        }
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    static func dismantleNSView(_ view: NSView, coordinator: Coordinator) {
        coordinator.poll?.cancel()
    }

    final class Coordinator {
        var poll: Task<Void, Never>?
    }
}
