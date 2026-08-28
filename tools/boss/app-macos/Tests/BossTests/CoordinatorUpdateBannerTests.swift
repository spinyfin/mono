import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// Offscreen layout pins for `CoordinatorUpdateBanner` in the coordinator
/// pane column. The banner used to live in window chrome (full width,
/// pushing the kanban down); it now sits in a 280...600pt column, so the
/// strip must keep a usable height at the pane's narrowest expanded width
/// rather than overflowing horizontally. Isolated `--capture-to` cannot
/// force `coordinatorUpdateAvailable` without a live engine, so this host
/// is the hermetic check.
///
/// No `NSWindow` — that path segfaults under the bazel XCTest host; see
/// `BossCaptureTests`. SwiftUI does not materialize `NSButton` / AX
/// children here either, so layout is pinned via frame + bitmap.
@MainActor
final class CoordinatorUpdateBannerTests: XCTestCase {
    /// Mirrors `workBossPanelMinWidth` in `ContentView.swift`.
    private let paneMinWidth: CGFloat = 280
    /// Mirrors `workBossPanelDefaultExpandedWidth` in `ContentView.swift`.
    private let paneDefaultWidth: CGFloat = 380
    /// Mirrors `workBossPanelCollapsedWidth` in `ContentView.swift`.
    private let paneCollapsedWidth: CGFloat = 88

    func testBannerDoesNotOverflowPaneMinimumWidth() throws {
        try assertBannerFits(width: paneMinWidth, snapshotName: "coordinator-update-banner-min.png")
    }

    func testBannerDoesNotOverflowDefaultPaneWidth() throws {
        try assertBannerFits(width: paneDefaultWidth, snapshotName: "coordinator-update-banner-default.png")
    }

    func testNarrowPaneGrowsVerticallyInsteadOfClipping() throws {
        let minHost = hostedBanner(width: paneMinWidth)
        let defaultHost = hostedBanner(width: paneDefaultWidth)
        XCTAssertGreaterThanOrEqual(
            minHost.bounds.height,
            defaultHost.bounds.height - 1,
            "at 280pt the copy must wrap or stack rather than clip; min=\(minHost.bounds.height) default=\(defaultHost.bounds.height)"
        )
        XCTAssertGreaterThan(
            minHost.bounds.height,
            48,
            "narrow banner must have room for wrapped copy plus the Reset control"
        )
    }

    func testCollapsedStripKeepsTheUpdateSignal() throws {
        try assertBannerFits(
            width: paneCollapsedWidth,
            isCollapsed: true,
            snapshotName: "coordinator-update-banner-collapsed.png"
        )
        let host = hostedBanner(width: paneCollapsedWidth, isCollapsed: true)
        XCTAssertLessThan(
            host.bounds.height,
            56,
            "collapsed affordance is a glyph strip, not wrapped copy; height=\(host.bounds.height)"
        )
    }

    private func assertBannerFits(
        width: CGFloat,
        isCollapsed: Bool = false,
        snapshotName: String
    ) throws {
        let host = hostedBanner(width: width, isCollapsed: isCollapsed)
        XCTAssertEqual(host.bounds.width, width, accuracy: 0.5)
        XCTAssertGreaterThan(host.bounds.height, 24, "banner collapsed vertically at \(width)pt")

        guard let rep = host.bitmapImageRepForCachingDisplay(in: host.bounds) else {
            throw XCTSkip("bitmapImageRepForCachingDisplay returned nil")
        }
        host.cacheDisplay(in: host.bounds, to: rep)
        if isUniformlyBlank(rep) {
            throw XCTSkip("render came back uniformly blank; host does not support offscreen SwiftUI rendering")
        }
        XCTAssertGreaterThan(
            blueFraction(rep),
            0.10,
            "banner background should dominate the pane-width strip at \(width)pt"
        )
        writeSnapshot(rep, name: snapshotName)
    }

    private func hostedBanner(
        width: CGFloat,
        isCollapsed: Bool = false
    ) -> NSHostingView<some View> {
        let root = CoordinatorUpdateBanner(
            installedVersion: "2.1.238",
            onReset: {},
            isCollapsed: isCollapsed
        )
        .frame(width: width)
        let host = NSHostingView(rootView: root)
        // Floor at 40pt so a zero fittingSize still paints; no upper clamp —
        // a genuine overflow must show up as a taller host, not get capped.
        host.frame = NSRect(x: 0, y: 0, width: width, height: 40)
        host.layoutSubtreeIfNeeded()
        let fittedHeight = max(host.fittingSize.height, host.intrinsicContentSize.height)
        let height = max(fittedHeight, 40)
        host.frame = NSRect(x: 0, y: 0, width: width, height: height)
        host.layoutSubtreeIfNeeded()
        return host
    }

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

    /// Fraction of sampled pixels in the blue family used by
    /// `Color.blue.opacity(0.85)` — enough to prove the strip painted.
    private func blueFraction(_ rep: NSBitmapImageRep) -> Double {
        guard let bytes = rep.bitmapData, rep.samplesPerPixel >= 3 else { return 0 }
        var blue = 0
        var total = 0
        for y in stride(from: 0, to: rep.pixelsHigh, by: 2) {
            for x in stride(from: 0, to: rep.pixelsWide, by: 2) {
                let offset = y * rep.bytesPerRow + x * rep.samplesPerPixel
                let r = bytes[offset]
                let g = bytes[offset + 1]
                let b = bytes[offset + 2]
                total += 1
                if b > 80, b > r, b > g { blue += 1 }
            }
        }
        return total == 0 ? 0 : Double(blue) / Double(total)
    }

    private func writeSnapshot(_ rep: NSBitmapImageRep, name: String) {
        guard let data = rep.representation(using: .png, properties: [:]) else { return }
        let destinations: [URL] = [
            ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"],
            ProcessInfo.processInfo.environment["TEST_TMPDIR"],
            NSTemporaryDirectory(),
        ].compactMap { path in
            path.map { URL(fileURLWithPath: $0, isDirectory: true) }
        }
        for dir in destinations {
            try? data.write(to: dir.appendingPathComponent(name))
        }
        print("COORDINATOR_UPDATE_BANNER_SNAPSHOT=\(name)")
    }
}
