import SwiftUI
import XCTest
@testable import Boss

/// The former "Work Error" modal path. `workErrorMessage` now feeds the
/// passive `WorkNoticeBanner` in the titlebar chrome: a message shows up
/// (and a newer one replaces it in place), an explicit dismiss clears it,
/// and nothing about presenting it changes which window or control has
/// focus. `ReportOnlyAlertScanTests` is the source-level half of this
/// guarantee (no `.alert` is bound to `workErrorMessage` anywhere).
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

    /// The banner is an ordinary view in the window chrome: evaluating it
    /// headlessly must not require, create, or key a window. Presenting an
    /// `NSAlert` or a SwiftUI `.alert` would have needed a key window to
    /// attach to (and would have become key itself).
    func testBannerRendersWithoutTakingAWindowOrFocus() {
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
}
