import CoreVideo
import XCTest
@testable import Boss

/// Pins the measured host-display contract used to diagnose
/// `ghostty_surface_new` NULL returns, and documents the CoreVideo
/// precondition that made locked-screen spawns fail on GhosttyKit 5659cef.
final class HostDisplaySnapshotTests: XCTestCase {
    func testSummaryNamesMeasuredFields() {
        let host = HostDisplaySnapshot.make(
            activeDisplayCount: 0,
            onlineDisplayCount: 1,
            mainDisplayAsleep: true,
            sessionLocked: true,
            sessionOnConsole: true,
            screenCount: 1,
            nsScreenMainNonNil: true,
            vsyncOverrideApplied: true
        )
        let summary = host.summary
        XCTAssertTrue(summary.contains("active_displays=0"), summary)
        XCTAssertTrue(summary.contains("online_displays=1"), summary)
        XCTAssertTrue(summary.contains("main_display_asleep=true"), summary)
        XCTAssertTrue(summary.contains("session_locked=true"), summary)
        XCTAssertTrue(summary.contains("ns_screen_main_non_nil=true"), summary)
        XCTAssertTrue(summary.contains("vsync_override_applied=true"), summary)
    }

    func testJsonObjectUsesSnakeCaseKeys() {
        let host = HostDisplaySnapshot.make(
            activeDisplayCount: 2,
            onlineDisplayCount: 2,
            mainDisplayAsleep: false,
            sessionLocked: false,
            screenCount: 2,
            nsScreenMainNonNil: true,
            vsyncOverrideApplied: false
        )
        let obj = host.jsonObject
        XCTAssertEqual(obj["active_display_count"] as? Int, 2)
        XCTAssertEqual(obj["online_display_count"] as? Int, 2)
        XCTAssertEqual(obj["main_display_asleep"] as? Bool, false)
        XCTAssertEqual(obj["session_locked"] as? Bool, false)
        XCTAssertEqual(obj["ns_screen_main_non_nil"] as? Bool, true)
        XCTAssertEqual(obj["vsync_override_applied"] as? Bool, false)
    }

    func testCodableRoundTrip() throws {
        let host = HostDisplaySnapshot.make(
            activeDisplayCount: 0,
            onlineDisplayCount: 1,
            mainDisplayAsleep: true,
            sessionLocked: true,
            sessionOnConsole: false,
            screenCount: 1,
            nsScreenMainNonNil: true,
            vsyncOverrideApplied: true
        )
        let data = try JSONEncoder().encode(host)
        let decoded = try JSONDecoder().decode(HostDisplaySnapshot.self, from: data)
        XCTAssertEqual(decoded, host)
        // Snake_case on the wire so spawn JSONL greps match tooling.
        let json = String(data: data, encoding: .utf8)!
        XCTAssertTrue(json.contains("active_display_count"))
        XCTAssertTrue(json.contains("vsync_override_applied"))
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
            nsScreenMainNonNil: true,
            vsyncOverrideApplied: false
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
        XCTAssertTrue(diagnostic.contains("host.vsync_override:   false"), diagnostic)
    }

    func testEmbedOverrideConfigDisablesWindowVsync() {
        // The Boss-side fix for locked-display surface creation: skip the
        // fatal DisplayLink create in GhosttyKit 5659cef by forcing
        // window-vsync=false on the embed config.
        let contents = GhosttyRuntime.embedOverrideConfigContents
        XCTAssertTrue(
            contents.contains("window-vsync = false"),
            "embed override must force window-vsync=false; got:\n\(contents)"
        )
        // Only the assignment line matters (comments may mention the key).
        let assignments = contents
            .split(separator: "\n")
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty && !$0.hasPrefix("#") }
        XCTAssertEqual(assignments, ["window-vsync = false"])
        // Header must state the file is regenerated, not hand-edited.
        XCTAssertTrue(
            contents.contains("Regenerated on every launch"),
            "override header must warn that edits are discarded; got:\n\(contents)"
        )
    }

    func testWriteEmbedOverrideConfigFileProducesFile() throws {
        let appSupport = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"]
                ?? NSTemporaryDirectory(),
            isDirectory: true
        )
        .appendingPathComponent("ghostty-override-test-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: appSupport) }

        let written = try GhosttyRuntime.writeEmbedOverrideConfigFile(
            applicationSupportDirectory: appSupport
        )
        XCTAssertTrue(
            FileManager.default.fileExists(atPath: written.path),
            "helper must create the override file at \(written.path)"
        )
        let onDisk = try String(contentsOf: written, encoding: .utf8)
        XCTAssertTrue(onDisk.contains("window-vsync = false"), onDisk)
        let assignments = onDisk
            .split(separator: "\n")
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty && !$0.hasPrefix("#") }
        XCTAssertEqual(assignments, ["window-vsync = false"])
        XCTAssertEqual(
            written.lastPathComponent,
            "ghostty-embed-overrides.config"
        )
        XCTAssertTrue(
            written.path.hasPrefix(appSupport.path),
            "helper must honor the Application Support override for tests"
        )
    }

    /// CoreVideo refuses a display link over an empty display set — the
    /// precondition GhosttyKit 5659cef treated as a hard surface-init error
    /// (`try DisplayLink.createWithActiveCGDisplays()`). Boss skips that
    /// path via `window-vsync = false`; this pins the OS-level rejection.
    func testCVDisplayLinkCreateWithZeroDisplaysFails() {
        var link: CVDisplayLink?
        // Count 0: CoreVideo must not create a link. A non-null buffer is
        // required by the Swift overlay even when count is zero.
        var unusedDisplay: CGDirectDisplayID = 0
        let status = CVDisplayLinkCreateWithCGDisplays(&unusedDisplay, 0, &link)
        XCTAssertNotEqual(
            status,
            kCVReturnSuccess,
            "empty display list must be rejected — this is the libghostty pin's fatal path"
        )
        XCTAssertNil(link)
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
