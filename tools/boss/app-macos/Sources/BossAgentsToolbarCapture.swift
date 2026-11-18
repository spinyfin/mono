import AppKit
import PaneLayout
import SwiftUI

/// Quiet real-toolbar fixture, with no terminal processes or engine writes.
@MainActor
enum BossAgentsToolbarCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let width: CGFloat = CommandLine.arguments.contains("--capture-narrow") ? 1000 : 1600
        let grid = AgentsGridModel(geometry: PaneGeometry(
            area: PaneSize(width: 900, height: 500), cell: AgentsGridModel.fallbackCell
        ))
        let states = (1...12).map { index in
            WorkerLiveState(
                name: "Agent \(index)", slotId: index, runId: "capture-\(index)",
                model: "fixture", shellPid: 0, lastEventAt: nil, currentTool: nil,
                lastToolEndedAt: nil, activity: index == 3 ? .waitingForInput : .working,
                liveStatus: nil, liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true,
                agentType: "coding", hostId: WorkerLiveState.localHostId, pool: "main"
            )
        }
        grid.apply(states: states, viewers: [:], interactivePoolSize: 16,
                   automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false)
        let view = HStack(spacing: 0) {
            Text("Agents").frame(width: 200, height: 240)
                .background(Color(nsColor: .controlBackgroundColor))
            VStack(spacing: 0) {
                AgentsToolbar(grid: grid, feedUnavailable: false)
                Divider()
                Color(nsColor: .windowBackgroundColor)
            }
        }
        .frame(width: width, height: 240)
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: width, height: 240),
            styleMask: [.titled], backing: .buffered, defer: false
        )
        captureWindow.title = "Boss [agent capture]"
        captureWindow.contentView = NSHostingView(rootView: view)
        window = captureWindow
        DispatchQueue.main.asyncAfter(deadline: .now() + BossCaptureArgs.shared.captureAfter) {
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                if CommandLine.arguments.contains("--capture-scroll-end") {
                    guard let content = captureWindow.contentView,
                          let scroll = overflowingScrollView(in: content),
                          let document = scroll.documentView else {
                        fputs("agents toolbar capture has no horizontal overflow scroll view\n", stderr)
                        exit(1)
                    }
                    scroll.contentView.scroll(to: NSPoint(
                        x: document.bounds.maxX - scroll.contentView.bounds.width, y: 0
                    ))
                    scroll.reflectScrolledClipView(scroll.contentView)
                    guard scroll.contentView.bounds.minX > 0 else {
                        fputs("agents toolbar did not scroll horizontally\n", stderr)
                        exit(1)
                    }
                    print("Toolbar scroll verified: content \(document.bounds.width), viewport \(scroll.contentView.bounds.width), offset \(scroll.contentView.bounds.minX)")
                }
                NSApp.terminate(nil)
            } catch {
                fputs("agents toolbar capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }

    private static func overflowingScrollView(in view: NSView) -> NSScrollView? {
        if let scroll = view as? NSScrollView,
           let document = scroll.documentView,
           document.bounds.width > scroll.contentView.bounds.width {
            return scroll
        }
        return view.subviews.lazy.compactMap { overflowingScrollView(in: $0) }.first
    }
}
