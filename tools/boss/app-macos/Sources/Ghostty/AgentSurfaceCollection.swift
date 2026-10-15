import PaneLayout
import SwiftUI

/// The sole owner of attached surface views. Neither page nor cell is an
/// ancestor identity, and invisible/unplaced runs remain in this collection.
struct AgentSurfaceCollection<Content: View>: View {
    let runIds: [String]
    // Value inputs expose page and geometry changes to SwiftUI diffing.
    let layout: PaneLayoutModel
    let area: CGSize
    @ViewBuilder let content: (String) -> Content

    var body: some View {
        ZStack(alignment: .topLeading) {
            ForEach(runIds, id: \.self) { runId in
                let location = placement(for: runId)
                content(runId)
                    .frame(width: location.frame.width, height: location.frame.height)
                    .position(x: location.frame.midX, y: location.frame.midY)
                    .opacity(location.visible ? 1 : 0)
                    .allowsHitTesting(location.visible)
                    .accessibilityHidden(!location.visible)
            }
        }
    }

    private func placement(for runId: String) -> (frame: CGRect, visible: Bool) {
        AgentsGridModel.placement(for: runId, area: area, layout: layout)
    }
}
