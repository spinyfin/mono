import SwiftUI
import XCTest
@testable import Boss

/// `workErrorMessage` feeds the passive `WorkNoticeBanner` in the titlebar
/// chrome: a message shows up (and a newer one replaces it in place) and an
/// explicit dismiss clears it. `ReportOnlyAlertScanTests` is the source-level
/// guarantee that no `.alert` is bound to `workErrorMessage`.
@MainActor
final class WorkNoticeBannerTests: XCTestCase {
    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    func testEngineWorkErrorSurfacesAsBannerMessage() {
        let model = makeModel()
        XCTAssertNil(model.workErrorMessage)
        model.applyEventForTest(.error(message: "engine busy, retry: read ordering deadline expired behind an earlier read"))
        XCTAssertEqual(
            model.workErrorMessage,
            "engine busy, retry: read ordering deadline expired behind an earlier read"
        )
    }

    func testRepeatedWorkErrorReplacesTheBannerTextInPlace() {
        let model = makeModel()
        model.applyEventForTest(.error(message: "database is locked"))
        model.applyEventForTest(.error(message: "database is locked"))
        model.applyEventForTest(.error(message: "engine busy, retry"))
        // One banner, latest text — never a queue of dialogs to click through.
        XCTAssertEqual(model.workErrorMessage, "engine busy, retry")
    }

    func testDismissClearsTheBanner() {
        let model = makeModel()
        model.applyEventForTest(.error(message: "database is locked"))
        model.dismissWorkErrorMessage()
        XCTAssertNil(model.workErrorMessage)
    }

    /// Layout check only: the banner lays out headlessly and its dismiss
    /// action fires. Focus behavior is covered by the titlebar-accessory
    /// tests below.
    func testBannerLaysOutAndDismisses() {
        let keyWindowBefore = NSApp?.keyWindow
        var dismissed = false
        let banner = WorkNoticeBanner(message: "database is locked", onDismiss: { dismissed = true })
        let host = NSHostingView(rootView: banner)
        host.frame = NSRect(x: 0, y: 0, width: 600, height: 60)
        host.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(host.fittingSize.height, 0, "banner must lay out headlessly")
        XCTAssertNil(host.window, "banner must not create or attach to a window on its own")
        XCTAssertTrue(NSApp?.keyWindow === keyWindowBefore, "banner must not change the key window")
        XCTAssertFalse(dismissed)
        banner.onDismiss()
        XCTAssertTrue(dismissed)
    }

    func testWorkErrorAloneCountsAsAChromeBanner() {
        func present(workError: Bool) -> Bool {
            chromeBannersPresent(
                isConnected: true,
                showConnectionLostBanner: false,
                supervisionState: .running,
                hasHealthIssues: false,
                hasDisconnectedActionNotice: false,
                hasWorkErrorMessage: workError,
                hasRollbackNotice: false
            )
        }
        XCTAssertFalse(present(workError: false))
        XCTAssertTrue(present(workError: true), "a work error must install the titlebar accessory")
    }

    /// Drives the production accessory path from a real `.workError` event:
    /// the banner is installed and visible, focus stays on the text field,
    /// and no sheet is attached through presentation, replacement, and
    /// dismissal.
    func testWorkErrorBannerInstallsWithoutMovingFocusOrAttachingSheets() throws {
        let model = makeModel()
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 900, height: 600),
            styleMask: [.titled, .closable, .resizable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        let field = NSTextField(frame: NSRect(x: 20, y: 20, width: 200, height: 24))
        window.contentView?.addSubview(field)
        XCTAssertTrue(window.makeFirstResponder(field))
        let responderBefore = window.firstResponder
        let host = TitlebarAccessoryHost()

        func refresh() {
            let message = model.workErrorMessage
            let present = chromeBannersPresent(
                isConnected: true,
                showConnectionLostBanner: false,
                supervisionState: .running,
                hasHealthIssues: false,
                hasDisconnectedActionNotice: false,
                hasWorkErrorMessage: message != nil,
                hasRollbackNotice: false
            )
            let content = AnyView(Group {
                if let message {
                    WorkNoticeBanner(message: message, onDismiss: { model.dismissWorkErrorMessage() })
                }
            })
            host.update(window: window, content: content, isPresented: present, contentHeight: present ? 40 : 0)
        }
        func assertQuiet(_ step: String) {
            XCTAssertTrue(window.firstResponder === responderBefore, "focus moved during \(step)")
            XCTAssertNil(window.attachedSheet, "sheet attached during \(step)")
            XCTAssertTrue(window.sheets.isEmpty, "sheet attached during \(step)")
        }

        model.applyEventForTest(.error(message: "database is locked"))
        refresh()
        let controller = try XCTUnwrap(host.installedController, "banner must be installed")
        XCTAssertFalse(controller.isHidden)
        assertQuiet("initial presentation")

        model.applyEventForTest(.error(message: "engine busy, retry"))
        refresh()
        XCTAssertFalse(try XCTUnwrap(host.installedController).isHidden)
        assertQuiet("replacement")

        model.dismissWorkErrorMessage()
        refresh()
        XCTAssertTrue(try XCTUnwrap(host.installedController).isHidden, "dismiss hides the banner")
        assertQuiet("dismissal")
    }
}
