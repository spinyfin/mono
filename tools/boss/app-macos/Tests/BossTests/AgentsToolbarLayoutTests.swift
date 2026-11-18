import AppKit
import PaneLayout
import SwiftUI
import XCTest
@testable import Boss

/// Hosts the real `AgentsToolbar` in an unpresented window to pin the
/// fits-vs-scroll behaviour against real layout geometry.
@MainActor
final class AgentsToolbarLayoutTests: XCTestCase {
    private func makeGrid() -> AgentsGridModel {
        let grid = AgentsGridModel(geometry: PaneGeometry(
            area: PaneSize(width: 900, height: 500), cell: AgentsGridModel.fallbackCell
        ))
        let states = (1...12).map { index in
            WorkerLiveState(
                name: "Agent \(index)", slotId: index, runId: "layout-\(index)",
                model: "fixture", shellPid: 0, lastEventAt: nil, currentTool: nil,
                lastToolEndedAt: nil, activity: index == 3 ? .waitingForInput : .working,
                liveStatus: nil, liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true,
                agentType: "coding", hostId: WorkerLiveState.localHostId, pool: "main"
            )
        }
        grid.apply(states: states, viewers: [:], interactivePoolSize: 16,
                   automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false)
        return grid
    }

    /// Retains both sides of AppKit's window/content-view relationship.
    private struct Hosted {
        let window: NSWindow
        let hosting: NSHostingView<AgentsToolbar>
    }

    /// Lays the toolbar out in a real, never-presented `NSWindow` (the
    /// pattern from `MarkdownTableOverflowTests`): a bare `NSHostingView`
    /// never materialises the horizontal `NSScrollView`.
    private func host(width: CGFloat) -> Hosted {
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: width, height: 400),
            styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        let hosting = NSHostingView(rootView: AgentsToolbar(grid: makeGrid(), feedUnavailable: false))
        hosting.frame = NSRect(x: 0, y: 0, width: width, height: 400)
        window.contentView = hosting
        for _ in 0..<12 {
            hosting.layoutSubtreeIfNeeded()
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        }
        XCTAssertFalse(window.isVisible, "layout window must never be presented")
        return Hosted(window: window, hosting: hosting)
    }

    private func scrollViews(in root: NSView) -> [NSScrollView] {
        var found: [NSScrollView] = []
        func walk(_ view: NSView) {
            if let scroll = view as? NSScrollView { found.append(scroll) }
            view.subviews.forEach(walk)
        }
        walk(root)
        return found
    }

    /// At a narrow width the controls overflow into a horizontal scroll view
    /// whose content is wider than its viewport, and scrolling to the limit
    /// reveals the trailing edge.
    func testNarrowWidthOverflowsIntoReachableHorizontalScroll() throws {
        let hosted = host(width: 360)
        let scroll = try XCTUnwrap(
            scrollViews(in: hosted.hosting).first, "narrow toolbar should fall back to a scroll view")
        let content = try XCTUnwrap(scroll.documentView)
        let clip = scroll.contentView.bounds.width
        XCTAssertGreaterThan(content.frame.width, clip + 1, "content should overflow the viewport")

        let overhang = max(0, content.frame.width - clip)
        scroll.contentView.scroll(to: NSPoint(x: overhang, y: scroll.contentView.bounds.minY))
        scroll.reflectScrolledClipView(scroll.contentView)
        XCTAssertGreaterThanOrEqual(
            scroll.documentVisibleRect.maxX, content.frame.width - 0.5,
            "trailing edge must be reachable")
    }

    /// At a wide width everything fits on one line with no overflowing scroll
    /// view, and the toolbar is a single caption row tall.
    func testWideWidthFitsOnOneLineWithoutOverflow() {
        let hosted = host(width: 1600)
        for scroll in scrollViews(in: hosted.hosting) {
            let content = scroll.documentView?.frame.width ?? 0
            XCTAssertLessThanOrEqual(
                content, scroll.contentView.bounds.width + 1, "wide toolbar should not overflow")
        }
        XCTAssertLessThan(hosted.hosting.fittingSize.height, 60)
    }
}
