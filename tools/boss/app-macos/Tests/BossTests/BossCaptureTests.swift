import AppKit
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
