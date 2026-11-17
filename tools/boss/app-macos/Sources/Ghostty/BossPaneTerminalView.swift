import AppKit
import SwiftUI

/// SwiftUI host for the Boss session's libghostty pane. Renders just
/// the terminal surface — pane chrome (title, collapse) lives in the
/// parent's single `bossAgentHeader` row.
struct BossPaneTerminalView: View {
    @ObservedObject var boss: BossPaneModel

    var body: some View {
        Group {
            if let session = boss.session {
                BossTerminalSurface(runtime: boss.runtime, session: session)
                    .id(session.id)
            } else {
                Color(nsColor: .black)
            }
        }
        .background(Color(nsColor: .black))
    }
}

private struct BossTerminalSurface: View {
    let runtime: GhosttyRuntime
    @ObservedObject var session: TerminalPaneSession

    var body: some View {
        GhosttyTerminalView(
            runtime: runtime,
            session: session,
            launchSpec: session.launchSpec,
            // Boss panes never display `paneMonitorState`, so the screen
            // scrape stays off.
            paneMonitorEnabled: false
        )
    }
}

final class GhosttyTerminalClaimView: NSView {}

/// Embeds an already-attached worker session without creating another PTY.
/// Claims the host while `isActive`; returns it to Agents when not.
struct EmbeddedWorkerTerminalView: NSViewRepresentable {
    let session: TerminalPaneSession
    let isActive: Bool

    @MainActor
    final class Coordinator {
        var session: TerminalPaneSession
        init(session: TerminalPaneSession) { self.session = session }
    }

    func makeCoordinator() -> Coordinator { Coordinator(session: session) }

    func makeNSView(context: Context) -> NSView {
        let container = GhosttyTerminalClaimView()
        container.wantsLayer = true
        container.layer?.backgroundColor = NSColor.black.cgColor
        return container
    }

    func updateNSView(_ container: NSView, context: Context) {
        context.coordinator.update(session: session, in: container, isActive: isActive)
    }

    static func dismantleNSView(_ container: NSView, coordinator: Coordinator) {
        coordinator.session.returnHostHome()
    }
}

extension EmbeddedWorkerTerminalView.Coordinator {
    func update(session: TerminalPaneSession, in container: NSView, isActive: Bool) {
        if self.session !== session {
            self.session.returnHostHome()
            self.session = session
        }
        if isActive {
            session.claimHost(into: container)
        } else if container.subviews.contains(where: { $0 === session.hostView }) {
            session.returnHostHome()
        }
    }
}
