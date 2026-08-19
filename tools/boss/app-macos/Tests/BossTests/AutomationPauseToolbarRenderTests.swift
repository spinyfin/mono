import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// Renders `AutomationPauseToolbarButton` in both states and both
/// appearances via a detached `NSHostingView` (no `NSWindow` — that
/// path segfaults under the bazel XCTest host; see `BossCaptureTests`).
/// Used to produce glanceable evidence that paused is an orange filled
/// capsule and running is a quiet outline.
@MainActor
final class AutomationPauseToolbarRenderTests: XCTestCase {
    func testPausedAndRunningRendersDifferInBothAppearances() throws {
        let temporaryDirectory = URL(
            fileURLWithPath: ProcessInfo.processInfo.environment["TEST_TMPDIR"] ?? NSTemporaryDirectory(),
            isDirectory: true
        )
        let dest = temporaryDirectory
            .appendingPathComponent("boss-auto-toolbar-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dest, withIntermediateDirectories: true)
        addTeardownBlock {
            try? FileManager.default.removeItem(at: dest)
        }

        let combinations: [(paused: Bool, appearance: NSAppearance.Name, name: String)] = [
            (false, .aqua, "running-light.png"),
            (false, .darkAqua, "running-dark.png"),
            (true, .aqua, "paused-light.png"),
            (true, .darkAqua, "paused-dark.png"),
        ]

        var orangeFractions: [String: Double] = [:]
        for combo in combinations {
            let rep = try render(paused: combo.paused, appearance: combo.appearance)
            let url = dest.appendingPathComponent(combo.name)
            guard let data = rep.representation(using: .png, properties: [:]) else {
                XCTFail("PNG encode failed for \(combo.name)")
                return
            }
            try data.write(to: url)

            guard !isUniformlyBlank(rep) else {
                throw XCTSkip("render came back uniformly blank; host does not support offscreen SwiftUI rendering")
            }
            orangeFractions[combo.name] = orangeFraction(rep)
        }

        // Write an index of the four PNG paths so they can be collected as review evidence.
        let index = dest.appendingPathComponent("paths.txt")
        try combinations.map { dest.appendingPathComponent($0.name).path }
            .joined(separator: "\n")
            .write(to: index, atomically: true, encoding: .utf8)
        print("AUTOMATION_PAUSE_TOOLBAR_FIXTURES=\(dest.path)")

        // Paused renders a filled orange capsule across a substantial share of the frame;
        // running is a quiet outline with essentially none.
        let minPausedOrangeFraction = 0.05
        let maxRunningOrangeFraction = 0.01
        XCTAssertGreaterThan(orangeFractions["paused-light.png"] ?? 0, minPausedOrangeFraction)
        XCTAssertGreaterThan(orangeFractions["paused-dark.png"] ?? 0, minPausedOrangeFraction)
        XCTAssertLessThan(orangeFractions["running-light.png"] ?? 1, maxRunningOrangeFraction)
        XCTAssertLessThan(orangeFractions["running-dark.png"] ?? 1, maxRunningOrangeFraction)
    }

    private func render(paused: Bool, appearance: NSAppearance.Name) throws -> NSBitmapImageRep {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        if paused {
            model.applyEventForTest(.engineHealthResult(
                apiKeyPresent: true,
                issues: [
                    EngineHealthIssue(
                        kind: EngineHealthIssue.automationPausedKind,
                        severity: "warning",
                        title: "Automations paused",
                        body: "…"
                    ),
                ]
            ))
        }
        model.isConnected = true

        let root = AutomationPauseToolbarButton(model: model)
            .padding(12)
            .background(Color(nsColor: .windowBackgroundColor))
            .frame(width: 160, height: 44)
        let host = NSHostingView(rootView: root)
        host.appearance = NSAppearance(named: appearance)
        host.frame = NSRect(x: 0, y: 0, width: 160, height: 44)
        host.layoutSubtreeIfNeeded()

        let bounds = host.bounds
        guard let rep = host.bitmapImageRepForCachingDisplay(in: bounds) else {
            throw XCTSkip("bitmapImageRepForCachingDisplay returned nil")
        }
        host.cacheDisplay(in: bounds, to: rep)
        return rep
    }

    /// True when every sampled pixel is identical, i.e. the host did not
    /// actually render the capsule (blank canvas rather than a real signal).
    private func isUniformlyBlank(_ rep: NSBitmapImageRep) -> Bool {
        guard let bytes = rep.bitmapData, rep.samplesPerPixel >= 3 else { return true }
        var first: [UInt8]?
        for y in stride(from: 0, to: rep.pixelsHigh, by: 4) {
            for x in stride(from: 0, to: rep.pixelsWide, by: 4) {
                let offset = y * rep.bytesPerRow + x * rep.samplesPerPixel
                let pixel = [bytes[offset], bytes[offset + 1], bytes[offset + 2]]
                if let seen = first {
                    if pixel != seen { return false }
                } else {
                    first = pixel
                }
            }
        }
        return true
    }

    /// Fraction of sampled pixels whose color falls within the systemOrange
    /// family (`Color.orange` fill used by the paused capsule), tolerant of
    /// antialiasing and appearance-driven blending.
    private func orangeFraction(_ rep: NSBitmapImageRep) -> Double {
        guard let bytes = rep.bitmapData, rep.samplesPerPixel >= 3 else { return 0 }
        var orangeCount = 0
        var sampleCount = 0
        for y in stride(from: 0, to: rep.pixelsHigh, by: 2) {
            for x in stride(from: 0, to: rep.pixelsWide, by: 2) {
                let offset = y * rep.bytesPerRow + x * rep.samplesPerPixel
                let red = bytes[offset]
                let green = bytes[offset + 1]
                let blue = bytes[offset + 2]
                sampleCount += 1
                if red > 200, green > 90, green < 210, blue < 100 {
                    orangeCount += 1
                }
            }
        }
        guard sampleCount > 0 else { return 0 }
        return Double(orangeCount) / Double(sampleCount)
    }
}
