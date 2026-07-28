import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// `ContentView` puts its chrome banners in the window's titlebar-accessory
/// slot so the main `NavigationSplitView` stays the window root (a wrapping
/// `VStack` demotes it and the sidebar renders as a floating inset panel).
/// These cover the accessory lifecycle that arrangement depends on.
@MainActor
final class TitlebarAccessoryHostTests: XCTestCase {
    private func makeWindow() -> NSWindow {
        NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 900, height: 600),
            styleMask: [.titled, .closable, .resizable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
    }

    private var banner: AnyView {
        AnyView(Color.orange.frame(height: 32))
    }

    func testDoesNotTouchTitlebarWhenNothingToPresent() {
        let window = makeWindow()
        let host = TitlebarAccessoryHost()

        host.update(window: window, content: banner, isPresented: false)

        XCTAssertTrue(window.titlebarAccessoryViewControllers.isEmpty)
        XCTAssertNil(host.installedController)
    }

    func testInstallsBottomAccessoryWhenPresented() throws {
        let window = makeWindow()
        let host = TitlebarAccessoryHost()

        host.update(window: window, content: banner, isPresented: true)

        XCTAssertEqual(window.titlebarAccessoryViewControllers.count, 1)
        let controller = try XCTUnwrap(host.installedController)
        XCTAssertEqual(controller.layoutAttribute, .bottom)
        XCTAssertFalse(controller.isHidden)
        // Must size from measured content; the system default clips expanded
        // multi-issue banners and is what left the sticky fullscreen band.
        XCTAssertFalse(controller.automaticallyAdjustsSize)
    }

    func testRepeatedUpdatesReuseTheSameAccessory() {
        let window = makeWindow()
        let host = TitlebarAccessoryHost()

        host.update(window: window, content: banner, isPresented: true)
        let first = host.installedController
        host.update(window: window, content: banner, isPresented: true)

        XCTAssertEqual(window.titlebarAccessoryViewControllers.count, 1)
        XCTAssertTrue(first === host.installedController)
    }

    func testHidesRatherThanRemovingWhenNoLongerPresented() {
        let window = makeWindow()
        let host = TitlebarAccessoryHost()

        host.update(window: window, content: banner, isPresented: true)
        host.update(window: window, content: banner, isPresented: false)

        XCTAssertEqual(window.titlebarAccessoryViewControllers.count, 1)
        XCTAssertEqual(host.installedController?.isHidden, true)

        host.update(window: window, content: banner, isPresented: true)
        XCTAssertEqual(host.installedController?.isHidden, false)
    }

    func testMovesAccessoryWhenReparentedToAnotherWindow() {
        let first = makeWindow()
        let second = makeWindow()
        let host = TitlebarAccessoryHost()

        host.update(window: first, content: banner, isPresented: true)
        host.update(window: second, content: banner, isPresented: true)

        XCTAssertTrue(first.titlebarAccessoryViewControllers.isEmpty)
        XCTAssertEqual(second.titlebarAccessoryViewControllers.count, 1)
    }

    func testDetachRemovesTheAccessory() {
        let window = makeWindow()
        let host = TitlebarAccessoryHost()

        host.update(window: window, content: banner, isPresented: true)
        host.detach()

        XCTAssertTrue(window.titlebarAccessoryViewControllers.isEmpty)
        XCTAssertNil(host.installedController)
    }

    /// The accessory hides with the titlebar in fullscreen unless it declares a
    /// minimum height, so every reported content height must land on the
    /// controller — including one the banner produces on its own (expanding to
    /// list every health issue) with no other state change alongside it.
    func testTracksContentHeightForFullScreen() throws {
        let window = makeWindow()
        let host = TitlebarAccessoryHost()

        host.update(window: window, content: banner, isPresented: true, contentHeight: 36)
        XCTAssertEqual(host.installedController?.fullScreenMinHeight, 36)

        host.update(window: window, content: banner, isPresented: true, contentHeight: 92)
        XCTAssertEqual(host.installedController?.fullScreenMinHeight, 92)
    }

    /// Windowed mode sizes the accessory from the view frame (with
    /// `automaticallyAdjustsSize == false`). Without this signal the slot
    /// keeps the collapsed height and an expanded multi-issue banner is
    /// clipped by AppKit's internal clip view.
    func testTracksContentHeightForWindowedFrame() throws {
        let window = makeWindow()
        let host = TitlebarAccessoryHost()

        host.update(window: window, content: banner, isPresented: true, contentHeight: 36)
        let container = try XCTUnwrap(host.installedContainerView)
        XCTAssertEqual(container.frame.height, 36, accuracy: 0.5)

        host.update(window: window, content: banner, isPresented: true, contentHeight: 92)
        XCTAssertEqual(container.frame.height, 92, accuracy: 0.5)
        XCTAssertEqual(host.installedController?.fullScreenMinHeight, 92)
    }

    /// Height is not sticky. A drop back to 0 (banner dismissed, or geometry
    /// not yet measured) must clear both the fullscreen reservation and the
    /// windowed frame so a window that once showed a banner does not keep a
    /// black menubar band forever.
    func testClearsHeightWhenUnmeasured() throws {
        let window = makeWindow()
        let host = TitlebarAccessoryHost()

        host.update(window: window, content: banner, isPresented: true, contentHeight: 36)
        host.update(window: window, content: banner, isPresented: true, contentHeight: 0)

        XCTAssertEqual(host.installedController?.fullScreenMinHeight, 0)
        let cleared = try XCTUnwrap(host.installedContainerView)
        XCTAssertEqual(cleared.frame.height, 0, accuracy: 0.5)
    }

    /// Hiding the accessory must also drop `fullScreenMinHeight` even if the
    /// last measured content height is still non-zero — otherwise a dismissed
    /// banner leaves a reserved fullscreen band.
    func testClearsFullScreenMinHeightWhenHidden() throws {
        let window = makeWindow()
        let host = TitlebarAccessoryHost()

        host.update(window: window, content: banner, isPresented: true, contentHeight: 36)
        XCTAssertEqual(host.installedController?.fullScreenMinHeight, 36)

        host.update(window: window, content: banner, isPresented: false, contentHeight: 36)
        XCTAssertEqual(host.installedController?.isHidden, true)
        XCTAssertEqual(host.installedController?.fullScreenMinHeight, 0)
        let hidden = try XCTUnwrap(host.installedContainerView)
        XCTAssertEqual(hidden.frame.height, 0, accuracy: 0.5)
    }

    func testIgnoresUpdatesBeforeTheViewHasAWindow() {
        let host = TitlebarAccessoryHost()

        host.update(window: nil, content: banner, isPresented: true)

        XCTAssertNil(host.installedController)
    }

    /// The container places the hosted banner at the window leading edge with
    /// full window width so the bar covers the band above a full-height
    /// sidebar, not only the content segment AppKit assigned the accessory.
    ///
    /// Exercised outside `NSTitlebarAccessoryViewController` because AppKit
    /// asserts if a live accessory view's origin is mutated directly.
    func testContainerSpansFullWindowWidthOnLayout() throws {
        let window = makeWindow()
        window.setFrame(NSRect(x: 100, y: 100, width: 900, height: 600), display: false)
        window.contentView?.frame = NSRect(x: 0, y: 0, width: 900, height: 600)

        let hosting = NSHostingView(rootView: banner)
        let container = TitlebarAccessoryContainer(hosting: hosting)
        // Simulate AppKit placing the accessory in the content segment (to the
        // right of a ~280pt sidebar).
        window.contentView?.addSubview(container)
        container.frame = NSRect(x: 280, y: 500, width: 620, height: 36)
        container.layoutSubtreeIfNeeded()

        // Leading edge of hosting should map to the window's leading edge.
        let hostingOriginInWindow = container.convert(hosting.frame.origin, to: nil)
        XCTAssertEqual(hostingOriginInWindow.x, 0, accuracy: 1.0)
        XCTAssertEqual(hosting.frame.width, window.frame.width, accuracy: 1.0)
        XCTAssertEqual(hosting.frame.height, 36, accuracy: 0.5)

        container.removeFromSuperview()
    }
}
