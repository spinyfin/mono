import AppKit
import PaneLayout
import SwiftUI
import XCTest
@testable import Boss

/// Hosts the real `AgentsToolbar` in a detached `NSHostingView` (no window;
/// see `BossCaptureTests`) to pin the fits-vs-scroll behaviour.
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

    private func host(width: CGFloat) -> NSHostingView<AgentsToolbar> {
        let view = NSHostingView(rootView: AgentsToolbar(grid: makeGrid(), feedUnavailable: false))
        view.frame = NSRect(x: 0, y: 0, width: width, height: 400)
        view.layoutSubtreeIfNeeded()
        return view
    }

    private func overflowingScrollView(in view: NSView) -> NSScrollView? {
        if let scroll = view as? NSScrollView,
           let document = scroll.documentView,
           document.bounds.width > scroll.contentView.bounds.width {
            return scroll
        }
        return view.subviews.lazy.compactMap { self.overflowingScrollView(in: $0) }.first
    }

    /// A detached hosting view does not materialise the horizontal
    /// NSScrollView (the capture fixture's `--capture-scroll-end` covers
    /// that), but it does expose the guarantee that controls never wrap:
    /// even at a narrow width the toolbar stays one caption row tall.
    func testNarrowWidthKeepsControlsOnOneLine() {
        let narrow = host(width: 360).fittingSize.height
        let wide = host(width: 1600).fittingSize.height
        XCTAssertLessThan(narrow, 60)
        XCTAssertEqual(narrow, wide, accuracy: 1)
    }

    func testWideWidthFitsWithoutScrolling() {
        let view = host(width: 1600)
        XCTAssertNil(overflowingScrollView(in: view), "wide toolbar should not need to scroll")
    }
}
