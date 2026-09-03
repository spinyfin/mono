import AppKit
import CryptoKit
import XCTest

@testable import Boss

/// Unit pins for the agent-capture helpers.
///
/// Full-app e2e still covers the live chrome path
/// (`BOSS_SOCKET_PATH=… BOSS_ENGINE_AUTOSTART=0 bazel run
/// //tools/boss/app-macos:Boss -- --capture-to …`). Creating an
/// `NSWindow` + `cacheDisplay` from this XCTest host under the bazel
/// macOS runner segfaults (reproduced 4/4); the hermetic pin below
/// exercises `cacheDisplay` on a detached `NSView` hierarchy instead —
/// same AppKit paint path, no window host.
final class BossCaptureTests: XCTestCase {
    func testParseCaptureArgs() {
        let parsed = BossCaptureArgs.parse([
            "Boss",
            "--capture-to", "/tmp/shot.png",
            "--capture-after", "3",
        ])
        XCTAssertEqual(parsed.captureTo, "/tmp/shot.png")
        XCTAssertEqual(parsed.captureAfter, 3, accuracy: 0.001)
        XCTAssertTrue(parsed.isCaptureMode)

        let equals = BossCaptureArgs.parse([
            "Boss",
            "--capture-to=/tmp/eq.png",
            "--capture-after=1.5",
        ])
        XCTAssertEqual(equals.captureTo, "/tmp/eq.png")
        XCTAssertEqual(equals.captureAfter, 1.5, accuracy: 0.001)

        let empty = BossCaptureArgs.parse(["Boss"])
        XCTAssertNil(empty.captureTo)
        XCTAssertFalse(empty.isCaptureMode)
        XCTAssertEqual(empty.captureAfter, BossCaptureArgs.defaultCaptureAfter, accuracy: 0.001)
    }

    func testSampleNonBlankPixelsDetectsSolidFill() {
        let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil,
            pixelsWide: 64,
            pixelsHigh: 64,
            bitsPerSample: 8,
            samplesPerPixel: 4,
            hasAlpha: true,
            isPlanar: false,
            colorSpaceName: .deviceRGB,
            bytesPerRow: 0,
            bitsPerPixel: 0
        )!
        // Mid blue via raw bytes (avoids setColor colour-space surprises).
        fill(rep, r: 30, g: 60, b: 200)
        let (nonBlank, total) = BossWindowCapture.sampleNonBlankPixels(rep)
        XCTAssertGreaterThan(total, 0)
        XCTAssertEqual(nonBlank, total)

        // Near-white must score zero non-blank.
        fill(rep, r: 250, g: 250, b: 250)
        let (whiteNonBlank, _) = BossWindowCapture.sampleNonBlankPixels(rep)
        XCTAssertEqual(whiteNonBlank, 0)

        // Near-black also zero.
        fill(rep, r: 5, g: 5, b: 5)
        let (blackNonBlank, _) = BossWindowCapture.sampleNonBlankPixels(rep)
        XCTAssertEqual(blackNonBlank, 0)
    }

    /// Pins the design contract that `cacheDisplay` paints real mid-tone
    /// content (including a badge-sized capsule region) into a bitmap —
    /// the same primitive the production capture path relies on — without
    /// constructing an `NSWindow` (which segfaults in this XCTest host).
    func testCacheDisplayOnDetachedViewProducesNonBlankBadgeRegion() {
        // Canvas large enough to mirror a toolbar strip; mid-blue fill is
        // "content" for sampleNonBlankPixels (not near-white / near-black).
        let root = SolidFillView(frame: NSRect(x: 0, y: 0, width: 320, height: 80))
        root.fillColor = NSColor(srgbRed: 0.12, green: 0.18, blue: 0.28, alpha: 1)

        // Badge-sized capsule region (~caption + horizontal/vertical padding
        // of the live "AGENT CAPTURE — isolated instance" toolbar badge).
        let badge = SolidFillView(frame: NSRect(x: 12, y: 24, width: 220, height: 28))
        badge.fillColor = NSColor(srgbRed: 0.95, green: 0.55, blue: 0.10, alpha: 1)
        root.addSubview(badge)

        root.layoutSubtreeIfNeeded()
        let bounds = root.bounds
        guard let rep = root.bitmapImageRepForCachingDisplay(in: bounds) else {
            XCTFail("bitmapImageRepForCachingDisplay returned nil")
            return
        }
        root.cacheDisplay(in: bounds, to: rep)

        let (nonBlank, total) = BossWindowCapture.sampleNonBlankPixels(rep)
        XCTAssertGreaterThan(total, 0, "sampler must visit pixels")
        let fraction = Double(nonBlank) / Double(total)
        // Production capture fails below 1% non-blank; a solid fill + badge
        // must clear that bar with room to spare.
        XCTAssertGreaterThan(
            fraction, 0.5,
            "cacheDisplay must paint mid-tone content (nonBlank fraction \(fraction))"
        )

        // Direct sample of the badge rect: every coarse-grid hit inside the
        // orange capsule must be non-white (the live capture badge is the
        // only chrome we can rely on under the glass-control blanking bug).
        let badgeNonWhite = countNonWhiteInRegion(
            rep,
            rect: NSRect(x: 12, y: 24, width: 220, height: 28)
        )
        XCTAssertGreaterThan(
            badgeNonWhite, 0,
            "badge-sized region must contain non-white pixels after cacheDisplay"
        )
    }

    private func fill(_ rep: NSBitmapImageRep, r: UInt8, g: UInt8, b: UInt8) {
        guard let bytes = rep.bitmapData else {
            XCTFail("bitmapData nil")
            return
        }
        let spp = rep.samplesPerPixel
        let bpr = rep.bytesPerRow
        for y in 0..<rep.pixelsHigh {
            for x in 0..<rep.pixelsWide {
                let o = y * bpr + x * spp
                bytes[o] = r
                bytes[o + 1] = g
                bytes[o + 2] = b
                if spp >= 4 { bytes[o + 3] = 255 }
            }
        }
    }

    /// Count coarse-grid samples inside `rect` that are not near-white.
    private func countNonWhiteInRegion(_ rep: NSBitmapImageRep, rect: NSRect) -> Int {
        let spp = rep.samplesPerPixel
        let bpr = rep.bytesPerRow
        guard let bytes = rep.bitmapData, spp >= 3, bpr > 0 else { return 0 }
        let x0 = max(0, Int(rect.minX.rounded(.down)))
        let y0 = max(0, Int(rect.minY.rounded(.down)))
        let x1 = min(rep.pixelsWide, Int(rect.maxX.rounded(.up)))
        let y1 = min(rep.pixelsHigh, Int(rect.maxY.rounded(.up)))
        guard x1 > x0, y1 > y0 else { return 0 }
        let step = max(1, min(x1 - x0, y1 - y0) / 16)
        var count = 0
        for y in stride(from: y0, to: y1, by: step) {
            for x in stride(from: x0, to: x1, by: step) {
                let o = y * bpr + x * spp
                let r = bytes[o]
                let g = bytes[o + 1]
                let b = bytes[o + 2]
                // Near-white threshold mirrors sampleNonBlankPixels (~0.92).
                if r < 235 || g < 235 || b < 235 {
                    count += 1
                }
            }
        }
        return count
    }

    func testDefaultProductionSocketConstant() {
        XCTAssertEqual(BossEnginePaths.defaultProductionSocket, "/tmp/boss-engine.sock")
    }

    func testCaptureSuiteName() {
        XCTAssertEqual(BossDefaults.captureSuiteName, "dev.spinyfin.bossmacapp.capture")
    }
}

/// Opaque fill view used only by the cacheDisplay hermetic pin.
private final class SolidFillView: NSView {
    var fillColor: NSColor = .systemBlue

    override var isOpaque: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        fillColor.setFill()
        bounds.fill()
    }
}
final class EngineProcessControllerTests: XCTestCase {
    func testReachableSocketRunsVersionCheckWhenPIDFileIsMissing() throws {
        let fixture = try Fixture(reachableSocket: .primary)
        let controller = fixture.makeController()
        defer { controller.stop() }

        try controller.start()

        XCTAssertEqual(fixture.socketControl.fingerprintRequests, [fixture.paths.socketPath])
        XCTAssertTrue(fixture.socketControl.shutdownRequests.isEmpty)
        XCTAssertFalse(FileManager.default.fileExists(atPath: fixture.paths.pidPath))
    }

    func testLegacySocketRunsVersionCheckWhenStateRootSocketIsMissing() throws {
        let fixture = try Fixture(reachableSocket: .legacy)
        let controller = fixture.makeController()
        defer { controller.stop() }

        try controller.start()

        XCTAssertEqual(fixture.socketControl.fingerprintRequests, [fixture.paths.legacySocketPath!])
        XCTAssertTrue(fixture.socketControl.shutdownRequests.isEmpty)
        XCTAssertFalse(FileManager.default.fileExists(atPath: fixture.paths.pidPath))
        XCTAssertFalse(FileManager.default.fileExists(atPath: fixture.paths.legacyPIDPath!))
    }

    func testStaleLegacyEngineIsStoppedBeforeReplacementUsesStateRootSocket() throws {
        let fixture = try Fixture(reachableSocket: .legacy, runningFingerprint: "stale-engine")
        let launchRecorder = LaunchRecorder()
        let controller = fixture.makeController { _, _, socketPath in
            launchRecorder.record(socketPath)
            return 4242
        }
        defer { controller.stop() }

        try controller.start()

        XCTAssertEqual(fixture.socketControl.shutdownRequests, [fixture.paths.legacySocketPath!])
        XCTAssertEqual(launchRecorder.socketPaths, [fixture.paths.socketPath])
    }

    func testUnresponsiveReachableEngineIsKeptWithoutReplacement() throws {
        let fixture = try Fixture(reachableSocket: .primary, fingerprintAvailable: false)
        let launchRecorder = LaunchRecorder()
        let controller = fixture.makeController { _, _, socketPath in
            launchRecorder.record(socketPath)
            return 4242
        }
        defer { controller.stop() }

        try controller.start()

        XCTAssertEqual(fixture.socketControl.fingerprintRequests, Array(repeating: fixture.paths.socketPath, count: 3))
        XCTAssertTrue(fixture.socketControl.shutdownRequests.isEmpty)
        XCTAssertTrue(launchRecorder.socketPaths.isEmpty)
    }

    func testFailedEngineStartPropagatesTheLaunchErrorToTheBannerState() throws {
        let fixture = try Fixture(reachableSocket: .none)
        let controller = fixture.makeController { _, _, _ in
            throw NSError(
                domain: "EngineFixture",
                code: 23,
                userInfo: [NSLocalizedDescriptionKey: "refusing-to-start-events-socket-owned"]
            )
        }
        defer { controller.stop() }
        let surfaced = expectation(description: "launch error surfaced")
        controller.onSupervisionStateChange = { state in
            if state == .restartFailed(attempt: nil, message: "refusing-to-start-events-socket-owned") {
                surfaced.fulfill()
            }
        }

        XCTAssertThrowsError(try controller.start()) { error in
            XCTAssertEqual(error.localizedDescription, "refusing-to-start-events-socket-owned")
        }
        wait(for: [surfaced], timeout: 1)
    }

    func testConnectedEngineAlwaysClearsUnreachableBanner() {
        XCTAssertFalse(
            shouldShowEngineUnreachableBanner(
                isConnected: true,
                showConnectionLostBanner: true,
                supervisionState: .gaveUp(attempts: 6, lastError: "refused to start")
            )
        )
    }

    func testFailedRestartHasAnUnreachableBannerSurface() {
        XCTAssertTrue(
            shouldShowEngineUnreachableBanner(
                isConnected: false,
                showConnectionLostBanner: false,
                supervisionState: .restartFailed(attempt: 2, message: "refused to start")
            )
        )
    }
}

private extension EngineProcessControllerTests {
    enum ReachableSocket: Equatable {
        case primary
        case legacy
        case none
    }

    final class FakeSocketControl: EngineSocketControlling, @unchecked Sendable {
        private let lock = NSLock()
        private let reachableSocket: String
        private let expectedFingerprint: String?
        private var requests: [String] = []
        private var shutdowns: [String] = []

        init(reachableSocket: String, expectedFingerprint: String?) {
            self.reachableSocket = reachableSocket
            self.expectedFingerprint = expectedFingerprint
        }

        var fingerprintRequests: [String] {
            lock.withLock { requests }
        }

        var shutdownRequests: [String] {
            lock.withLock { shutdowns }
        }

        func isReachable(socketPath: String, timeoutSeconds _: Double) -> Bool {
            socketPath == reachableSocket
        }

        func peerPID(socketPath _: String, timeoutSeconds _: Double) -> pid_t? {
            nil
        }

        func fingerprint(socketPath: String, timeoutSeconds _: Double) -> String? {
            lock.withLock { requests.append(socketPath) }
            return expectedFingerprint
        }

        func shutdown(socketPath: String, tokenPath _: String, timeoutSeconds _: Double) throws -> pid_t? {
            lock.withLock { shutdowns.append(socketPath) }
            return nil
        }

        func waitForClose(socketPath _: String, timeoutSeconds _: Double) -> Bool {
            true
        }
    }

    final class LaunchRecorder: @unchecked Sendable {
        private let lock = NSLock()
        private var recordedSocketPaths: [String] = []

        var socketPaths: [String] {
            lock.withLock { recordedSocketPaths }
        }

        func record(_ socketPath: String) {
            lock.withLock { recordedSocketPaths.append(socketPath) }
        }
    }

    struct Fixture {
        let temp: URL
        let paths: BossEnginePaths
        let bundledEnginePath: String
        let socketControl: FakeSocketControl

        init(
            reachableSocket: ReachableSocket,
            runningFingerprint: String? = nil,
            fingerprintAvailable: Bool = true
        ) throws {
            let testRoot = ProcessInfo.processInfo.environment["TEST_TMPDIR"]
                .map { URL(fileURLWithPath: $0, isDirectory: true) }
                ?? FileManager.default.temporaryDirectory
            temp = testRoot
                .appendingPathComponent(UUID().uuidString, isDirectory: true)
            try FileManager.default.createDirectory(at: temp, withIntermediateDirectories: true)
            let primarySocket = temp.appendingPathComponent("engine.sock").path
            let legacySocket = temp.appendingPathComponent("legacy.sock").path
            paths = BossEnginePaths(
                socketPath: primarySocket,
                pidPath: temp.appendingPathComponent("engine.pid").path,
                controlTokenPath: temp.appendingPathComponent("engine-control.token").path,
                legacySocketPath: legacySocket,
                legacyPIDPath: temp.appendingPathComponent("legacy.pid").path
            )
            bundledEnginePath = temp.appendingPathComponent("bundled-engine").path
            let contents = Data("bundled engine fixture".utf8)
            try contents.write(to: URL(fileURLWithPath: bundledEnginePath))
            let fingerprint = SHA256.hash(data: contents).prefix(6)
                .map { String(format: "%02x", $0) }
                .joined()
            let reachablePath: String
            switch reachableSocket {
            case .primary: reachablePath = primarySocket
            case .legacy: reachablePath = legacySocket
            case .none: reachablePath = temp.appendingPathComponent("unreachable.sock").path
            }
            socketControl = FakeSocketControl(
                reachableSocket: reachablePath,
                expectedFingerprint: fingerprintAvailable ? (runningFingerprint ?? fingerprint) : nil
            )
        }

        func makeController(
            launchHandler: (@Sendable (String, String?, String) throws -> pid_t)? = nil
        ) -> EngineProcessController {
            EngineProcessController(
                paths: paths,
                launchDirectory: temp.path,
                forceRestart: false,
                stopOnExit: false,
                restartPolicy: .default,
                socketControl: socketControl,
                bundledEnginePathOverride: bundledEnginePath,
                launchHandler: launchHandler
            )
        }
    }
}
