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

    func testIgnoresUnmeasuredContentHeight() throws {
        let window = makeWindow()
        let host = TitlebarAccessoryHost()

        host.update(window: window, content: banner, isPresented: true, contentHeight: 36)
        host.update(window: window, content: banner, isPresented: true, contentHeight: 0)

        XCTAssertEqual(host.installedController?.fullScreenMinHeight, 36)
    }

    func testIgnoresUpdatesBeforeTheViewHasAWindow() {
        let host = TitlebarAccessoryHost()

        host.update(window: nil, content: banner, isPresented: true)

        XCTAssertNil(host.installedController)
    }
}
