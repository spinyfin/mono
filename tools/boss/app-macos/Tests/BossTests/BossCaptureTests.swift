import AppKit
import XCTest

@testable import Boss

/// Unit pins for the agent-capture helpers.
///
/// The "hidden never-ordered window still paints under `cacheDisplay`"
/// contract is validated by the end-to-end isolated launch
/// (`BOSS_SOCKET_PATH=… BOSS_ENGINE_AUTOSTART=0 bazel run
/// //tools/boss/app-macos:Boss -- --capture-to …`). Creating an
/// `NSWindow` + `cacheDisplay` from this XCTest host under the bazel
/// macOS runner segfaults (reproduced 4/4); do not reintroduce that
/// surface here.
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

    func testDefaultProductionSocketConstant() {
        XCTAssertEqual(BossEnginePaths.defaultProductionSocket, "/tmp/boss-engine.sock")
    }

    func testCaptureSuiteName() {
        XCTAssertEqual(BossDefaults.captureSuiteName, "dev.spinyfin.bossmacapp.capture")
    }
}
