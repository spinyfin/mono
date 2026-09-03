import XCTest

@testable import Boss

/// A worker's terminal bell must never ring the operator's system alert —
/// only the operator's own pane (`role == .boss`) may. libghostty forwards
/// `GHOSTTY_ACTION_RING_BELL` for every BEL from every mounted surface,
/// including hidden worker panes kept live at `opacity(0)`, with no gate of
/// its own; `GhosttyRuntime.shouldRingBell` is the app-side gate.
final class GhosttyRuntimeRingBellTests: XCTestCase {
    func testWorkerPaneDoesNotRingBell() {
        XCTAssertFalse(GhosttyRuntime.shouldRingBell(role: .worker(slot: 1)))
    }

    func testBossPaneRingsBell() {
        XCTAssertTrue(GhosttyRuntime.shouldRingBell(role: .boss))
    }

    func testUnresolvedHostDoesNotRingBell() {
        XCTAssertFalse(GhosttyRuntime.shouldRingBell(role: nil))
    }
}
