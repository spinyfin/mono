import SwiftUI

/// The sole owner of attached surface views. Neither page nor cell is an
/// ancestor identity, and invisible/unplaced runs remain in this collection.
struct AgentSurfaceCollection<Content: View>: View {
    let runIds: [String]
    let placement: (String) -> (frame: CGRect, visible: Bool)
    @ViewBuilder let content: (String) -> Content

    var body: some View {
        ZStack(alignment: .topLeading) {
            ForEach(runIds, id: \.self) { runId in
                let location = placement(runId)
                content(runId)
                    .frame(width: location.frame.width, height: location.frame.height)
                    .position(x: location.frame.midX, y: location.frame.midY)
                    .opacity(location.visible ? 1 : 0)
                    .allowsHitTesting(location.visible)
                    .accessibilityHidden(!location.visible)
            }
        }
    }
}
