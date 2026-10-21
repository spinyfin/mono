import SwiftUI

/// Keeps the native terminal at one structural position across membership
/// changes. Only removing the attached session dismantles its surface.
struct WorkerSlotBody<Terminal: View, Placeholder: View, ViewerMissing: View>: View {
    let snapshot: WorkerSlotSnapshot
    @ViewBuilder let terminal: (TerminalPaneSession) -> Terminal
    @ViewBuilder let placeholder: () -> Placeholder
    @ViewBuilder let viewerMissing: () -> ViewerMissing

    var body: some View {
        ZStack {
            if let session = snapshot.session {
                terminal(session)
                    .id(session.id)
                    .opacity(snapshot.bodyKind == .terminal ? (snapshot.isExited ? 0.55 : 1) : 0)
                    .allowsHitTesting(snapshot.bodyKind == .terminal)
                    .accessibilityHidden(snapshot.bodyKind != .terminal)
            }
            switch snapshot.bodyKind {
            case .placeholder:
                placeholder()
            case .viewerMissing:
                viewerMissing()
            case .terminal:
                EmptyView()
            }
        }
    }
}
