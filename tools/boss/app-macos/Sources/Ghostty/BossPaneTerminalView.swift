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
