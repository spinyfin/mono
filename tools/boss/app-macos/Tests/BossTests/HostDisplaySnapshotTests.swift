import AppKit
import CoreVideo
import XCTest
@testable import Boss

/// Pins the measured host-display contract used to diagnose
/// `ghostty_surface_new` NULL returns, and documents the CoreVideo
/// condition GhosttyKit now tolerates during locked-screen spawns.
final class HostDisplaySnapshotTests: XCTestCase {
    func testSummaryNamesMeasuredFields() {
        let host = HostDisplaySnapshot.make(
            activeDisplayCount: 0,
            onlineDisplayCount: 1,
            mainDisplayAsleep: true,
            sessionLocked: true,
            sessionOnConsole: true,
            screenCount: 1,
            nsScreenMainNonNil: true
        )
        let summary = host.summary
        XCTAssertTrue(summary.contains("active_displays=0"), summary)
        XCTAssertTrue(summary.contains("online_displays=1"), summary)
        XCTAssertTrue(summary.contains("main_display_asleep=true"), summary)
        XCTAssertTrue(summary.contains("session_locked=true"), summary)
        XCTAssertTrue(summary.contains("ns_screen_main_non_nil=true"), summary)
    }

    func testJsonObjectUsesSnakeCaseKeys() {
        let host = HostDisplaySnapshot.make(
            activeDisplayCount: 2,
            onlineDisplayCount: 2,
            mainDisplayAsleep: false,
            sessionLocked: false,
            screenCount: 2,
            nsScreenMainNonNil: true
        )
        let obj = host.jsonObject
        XCTAssertEqual(obj["active_display_count"] as? Int, 2)
        XCTAssertEqual(obj["online_display_count"] as? Int, 2)
        XCTAssertEqual(obj["main_display_asleep"] as? Bool, false)
        XCTAssertEqual(obj["session_locked"] as? Bool, false)
        XCTAssertEqual(obj["ns_screen_main_non_nil"] as? Bool, true)
    }

    func testCodableRoundTrip() throws {
        let host = HostDisplaySnapshot.make(
            activeDisplayCount: 0,
            onlineDisplayCount: 1,
            mainDisplayAsleep: true,
            sessionLocked: true,
            sessionOnConsole: false,
            screenCount: 1,
            nsScreenMainNonNil: true
        )
        let data = try JSONEncoder().encode(host)
        let decoded = try JSONDecoder().decode(HostDisplaySnapshot.self, from: data)
        XCTAssertEqual(decoded, host)
        // Snake_case on the wire so spawn JSONL greps match tooling.
        let json = String(data: data, encoding: .utf8)!
        XCTAssertTrue(json.contains("active_display_count"))
        XCTAssertFalse(json.contains("activeDisplayCount"))
    }

    /// Documents the false-positive that attributed the failure to a
    /// non-display cause and pointed at a stderr dump that is not
    /// retrievable: AppKit can report a main screen while CG has zero
    /// active displays (lock screen / display sleep).
    func testSurfaceFailureReasonDoesNotTrustNSScreenMainAlone() {
        // Locked + asleep, CG active=0, but NSScreen.main still non-nil —
        // the 2026-08-10 failure shape.
        let lockedAsleep = HostDisplaySnapshot.make(
            activeDisplayCount: 0,
            onlineDisplayCount: 1,
            mainDisplayAsleep: true,
            sessionLocked: true,
            screenCount: 1,
            nsScreenMainNonNil: true
        )
        let reason = GhosttyTerminalHostView.surfaceFailureReason(host: lockedAsleep)
        XCTAssertTrue(reason.contains("no active CG displays"), reason)
        XCTAssertTrue(reason.contains("session_locked=true"), reason)
        XCTAssertTrue(reason.contains("main_display_asleep=true"), reason)
        // Must not claim a display is active just because NSScreen.main is set.
        XCTAssertFalse(
            reason.contains("a display IS active"),
            "NSScreen.main non-nil must not produce the false-active NACK; got: \(reason)"
        )
        XCTAssertFalse(
            reason.contains("display availability is not the cause"),
            "zero active CG displays is the cause; got: \(reason)"
        )
    }

    func testSurfaceFailureReasonWithActiveDisplaysPointsAtBossctlLogs() {
        let host = HostDisplaySnapshot.make(
            activeDisplayCount: 1,
            onlineDisplayCount: 1,
            screenCount: 1,
            nsScreenMainNonNil: true
        )
        let reason = GhosttyTerminalHostView.surfaceFailureReason(host: host)
        XCTAssertTrue(reason.contains("active CG displays present"), reason)
        XCTAssertTrue(reason.contains("active_displays=1"), reason)
        XCTAssertTrue(reason.contains("bossctl logs spawn"), reason)
        XCTAssertFalse(reason.contains("no active CG displays"), reason)
        // App fd 2 is /dev/null in production — never point at stderr.
        XCTAssertFalse(
            reason.lowercased().contains("stderr"),
            "reason must not reference unreadable stderr; got: \(reason)"
        )
    }

    func testDiagnosticIncludesHostFields() {
        let host = HostDisplaySnapshot.make(
            activeDisplayCount: 0,
            onlineDisplayCount: 1,
            mainDisplayAsleep: true,
            sessionLocked: true,
            screenCount: 1,
            nsScreenMainNonNil: true
        )
        let diagnostic = GhosttyTerminalHostView.surfaceFailureDiagnostic(
            appNonNil: true,
            workingDirectory: "/tmp/workdir",
            cwdExists: true,
            isDirectory: true,
            fontSize: 13,
            scaleFactor: 2.0,
            envVarCount: 1,
            envSummary: "PATH=/usr/bin",
            initialInputCount: 3,
            host: host
        )
        XCTAssertTrue(diagnostic.contains("host.active_displays:  0"), diagnostic)
        XCTAssertTrue(diagnostic.contains("host.session_locked:   true"), diagnostic)
        XCTAssertTrue(diagnostic.contains("host.main_asleep:      true"), diagnostic)
        XCTAssertTrue(diagnostic.contains("host.ns_main_non_nil:  true"), diagnostic)
    }

    /// CoreVideo refuses a display link over an empty display set. GhosttyKit
    /// now treats that failure as optional and continues surface creation.
    func testCVDisplayLinkCreateWithZeroDisplaysFails() {
        var link: CVDisplayLink?
        // Count 0: CoreVideo must not create a link. A non-null buffer is
        // required by the Swift overlay even when count is zero.
        var unusedDisplay: CGDirectDisplayID = 0
        let status = CVDisplayLinkCreateWithCGDisplays(&unusedDisplay, 0, &link)
        XCTAssertNotEqual(
            status,
            kCVReturnSuccess,
            "empty display list must be rejected so GhosttyKit must use its fallback path"
        )
        XCTAssertNil(link)
    }

    /// Exercises the C surface path against the pinned GhosttyKit build.
    /// With the display-link fix, this remains valid when CoreGraphics has
    /// no active displays: Ghostty falls back to change-driven rendering
    /// instead of rejecting the surface.
    @MainActor
    func testGhosttyKitCreatesSurfaceInWindow() {
        let host = HostDisplaySnapshot.capture()
        if ProcessInfo.processInfo.environment["BOSS_TEST_EXPECT_NO_ACTIVE_DISPLAY"] == "1" {
            XCTAssertEqual(
                host.activeDisplayCount,
                0,
                "display-sleep verification must begin with no active CoreGraphics displays"
            )
        }

        let launchSpec = TerminalLaunchSpec(
            fontSize: 12,
            workingDirectory: NSTemporaryDirectory(),
            initialInput: "exit\\n"
        )
        let session = TerminalPaneSession(
            id: "ghosttykit-surface-regression",
            role: .worker(slot: 0),
            launchSpec: launchSpec
        )
        let view = GhosttyTerminalHostView(
            runtime: GhosttyRuntime.shared,
            session: session,
            launchSpec: launchSpec,
            paneMonitorEnabled: false
        )
        defer { view.tearDown() }

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 640, height: 480),
            styleMask: [.titled],
            backing: .buffered,
            defer: false
        )
        window.contentView = view
        view.syncGeometry()

        XCTAssertNotNil(view.surface)
        XCTAssertTrue(session.terminalReady)
        XCTAssertTrue(view.window === window)
    }

    @MainActor
    func testLiveCaptureReturnsNonNegativeCounts() {
        let host = HostDisplaySnapshot.capture()
        XCTAssertGreaterThanOrEqual(host.activeDisplayCount, 0)
        XCTAssertGreaterThanOrEqual(host.onlineDisplayCount, 0)
        XCTAssertGreaterThanOrEqual(host.screenCount, 0)
        // Capture should be JSON-encodable for spawn logs.
        XCTAssertNoThrow(try JSONEncoder().encode(host))
    }
}
