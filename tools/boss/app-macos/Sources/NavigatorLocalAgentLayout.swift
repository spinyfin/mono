import SwiftUI

/// Observe attachment even while collapsed so a late viewer expands the pane.
struct NavigatorLocalAgentLayout: View {
    let context: NavigatorDetailContext
    let selected: NavigatorActivity
    let ended: Bool
    let isLive: Bool
    let isActive: Bool
    @ObservedObject var workspace: WorkersWorkspaceModel

    var hasAgentPane: Bool {
        workspace.viewer(forRunId: selected.runId ?? selected.executionId)?.session?.hostView != nil
    }

    var body: some View {
        if let session = workspace.viewer(forRunId: selected.runId ?? selected.executionId)?.session {
            NavigatorHostedAgentLayout(layout: self, session: session)
        } else {
            region(hasAgentPane: false)
        }
    }

    @ViewBuilder
    func region(hasAgentPane: Bool) -> some View {
        if hasAgentPane {
            VSplitView {
                WorkNavigatorBrief(context: context).frame(minHeight: 120)
                slot.frame(minHeight: 140)
            }
        } else {
            VStack(alignment: .leading, spacing: 8) {
                WorkNavigatorBrief(context: context).frame(maxHeight: .infinity)
                slot
            }
        }
    }

    // Both layouts mount the slot to keep its attach requests active.
    private var slot: some View {
        WorkNavigatorTerminalSlot(
            selected: selected, ended: ended, isLive: isLive,
            isActive: isActive, workspace: workspace
        )
    }
}

/// Host creation publishes on the session, independently of viewer membership.
private struct NavigatorHostedAgentLayout: View {
    let layout: NavigatorLocalAgentLayout
    @ObservedObject var session: TerminalPaneSession

    var body: some View {
        layout.region(hasAgentPane: session.hostView != nil)
    }
}
