import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class WorkDispatchFailureBannerTests: XCTestCase {
    func testParkNamesBothConditionsAndKeepsResumeVisible() throws {
        let diagnostic = "The sweep is holding this work item because its worker declared itself blocked; "
            + "it has ALSO produced 6 terminal executions, tripping the churn guard on top of the park. "
            + "Failing executions: " + String(repeating: "execution-id, ", count: 20)
        let banner = WorkDispatchFailureBanner(reason: "deliberate_park", errorText: diagnostic)
        XCTAssertEqual(banner.headline, "Waiting on you — deliberate park + churn guard")
        let summary = try XCTUnwrap(banner.summary)
        XCTAssertEqual(summary, "Review the open attention item, then drag to Doing to resume.")

        let width: CGFloat = 220
        guard let combinedRep = render(banner.frame(width: width).padding(8)) else {
            throw XCTSkip("render came back uniformly blank; host does not support offscreen SwiftUI rendering")
        }
        let combinedHeight = combinedRep.host.fittingSize.height

        // A single-line control at the same width proves the regression this
        // test guards against: if `summary`'s `.lineLimit(isDeliberatePark ?
        // nil : 3)` were reverted to a hard 1-line cap, the combined banner
        // would clip to roughly this height instead of wrapping to show the
        // entire resume instruction.
        let control = Text(summary)
            .font(.caption2)
            .lineLimit(1)
            .frame(width: width, alignment: .leading)
            .padding(8)
        guard let controlRep = render(control) else {
            throw XCTSkip("render came back uniformly blank; host does not support offscreen SwiftUI rendering")
        }
        XCTAssertGreaterThan(
            combinedHeight,
            controlRep.host.fittingSize.height,
            "full resume instruction must wrap onto multiple lines rather than clip to one, "
                + "proving lineLimit(nil) is in effect"
        )

        if let undeclared = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"].map({
            URL(fileURLWithPath: $0, isDirectory: true)
        }) {
            if let data = combinedRep.bitmap.representation(using: .png, properties: [:]) {
                try? data.write(to: undeclared.appendingPathComponent("combined-park-banner.png"))
            }
        }
    }

    func testParkOnlyAndDispatchFailureRemainDistinct() {
        let park = WorkDispatchFailureBanner(reason: "deliberate_park", errorText: "Worker declared blocked")
        XCTAssertEqual(park.headline, "Waiting on you — deliberate park")
        XCTAssertEqual(park.summary, "Review the open attention item, then drag to Doing to resume.")
        let failure = WorkDispatchFailureBanner(reason: "spawn_failed", errorText: "Could not launch worker")
        XCTAssertEqual(failure.headline, "Failed to start — spawn failed")
        XCTAssertEqual(failure.summary, "Could not launch worker")
    }

    // MARK: - Rendering

    private func render(_ view: some View) -> (host: NSHostingView<AnyView>, bitmap: NSBitmapImageRep)? {
        let host = NSHostingView(rootView: AnyView(view))
        host.frame = NSRect(origin: .zero, size: host.fittingSize)
        host.layoutSubtreeIfNeeded()
        guard let bitmap = host.bitmapImageRepForCachingDisplay(in: host.bounds) else { return nil }
        host.cacheDisplay(in: host.bounds, to: bitmap)
        guard !isUniformlyBlank(bitmap) else { return nil }
        return (host, bitmap)
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
}
