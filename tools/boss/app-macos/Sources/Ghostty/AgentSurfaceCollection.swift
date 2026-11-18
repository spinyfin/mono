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
        for page in layout.pages {
            for cell in page.cells {
                let id: String?
                switch cell.content {
                case .occupied(let member): id = member.runId
                case .placeholder(let run): id = run
                case .empty: id = nil
                }
                if id == runId {
                    let width = max(1, (area.width - CGFloat(page.shape.columns - 1)) / CGFloat(page.shape.columns))
                    let height = max(1, (area.height - CGFloat(page.shape.rows - 1)) / CGFloat(page.shape.rows))
                    let position = page.shape.position(ofLocalIndex: cell.localIndex)
                    return (CGRect(x: CGFloat(position.column) * (width + 1), y: CGFloat(position.row) * (height + 1), width: width, height: height), page.index == layout.selectedPage)
                }
            }
        }
        // Unplaced viewers still need a usable terminal size.
        return (CGRect(x: 0, y: 0, width: max(420, area.width), height: max(356, area.height)), false)
    }
}
