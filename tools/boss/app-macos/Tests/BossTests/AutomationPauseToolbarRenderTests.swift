import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// Renders `AutomationPauseToolbarButton` in both states and both
/// appearances via a detached `NSHostingView` (no `NSWindow` — that
/// path segfaults under the bazel XCTest host; see `BossCaptureTests`).
/// Used to attach glanceable evidence that paused is an orange filled
/// capsule and running is a quiet outline.
@MainActor
final class AutomationPauseToolbarRenderTests: XCTestCase {
    func testPausedAndRunningRendersDifferInBothAppearances() throws {
        let dest = FileManager.default.temporaryDirectory
            .appendingPathComponent("boss-auto-toolbar-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dest, withIntermediateDirectories: true)

        let combinations: [(paused: Bool, appearance: NSAppearance.Name, name: String)] = [
            (false, .aqua, "running-light.png"),
            (false, .darkAqua, "running-dark.png"),
            (true, .aqua, "paused-light.png"),
            (true, .darkAqua, "paused-dark.png"),
        ]

        var samples: [String: [UInt8]] = [:]
        for combo in combinations {
            let rep = try render(paused: combo.paused, appearance: combo.appearance)
            let url = dest.appendingPathComponent(combo.name)
            guard let data = rep.representation(using: .png, properties: [:]) else {
                XCTFail("PNG encode failed for \(combo.name)")
                return
            }
            try data.write(to: url)
            samples[combo.name] = sampleCenter(rep)
        }

        // Persist the four paths so a worker can `boss attach` them.
        let index = dest.appendingPathComponent("paths.txt")
        try combinations.map { dest.appendingPathComponent($0.name).path }
            .joined(separator: "\n")
            .write(to: index, atomically: true, encoding: .utf8)
        print("AUTOMATION_PAUSE_TOOLBAR_FIXTURES=\(dest.path)")

        XCTAssertNotEqual(samples["running-light.png"], samples["paused-light.png"])
        XCTAssertNotEqual(samples["running-dark.png"], samples["paused-dark.png"])
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

    private func sampleCenter(_ rep: NSBitmapImageRep) -> [UInt8] {
        let x = max(0, rep.pixelsWide / 2)
        let y = max(0, rep.pixelsHigh / 2)
        guard let bytes = rep.bitmapData, rep.samplesPerPixel >= 3 else { return [] }
        let offset = y * rep.bytesPerRow + x * rep.samplesPerPixel
        return [bytes[offset], bytes[offset + 1], bytes[offset + 2]]
    }
}
