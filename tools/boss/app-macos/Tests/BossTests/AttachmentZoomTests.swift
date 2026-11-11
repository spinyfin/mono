import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class AttachmentZoomTests: XCTestCase {
    func testSwitchingScreenshotsResetsToFitEvenWithIdenticalBytes() async throws {
        let digest = UUID().uuidString
        func attachment(_ id: String) -> AttachmentVM {
            AttachmentVM(id: id, executionId: "zoom-test", workItemId: "zoom-test", caption: "Tall image",
                         contentDigest: digest, createdAt: "0", mediaType: "png", pixelWidth: 390,
                         pixelHeight: 6245, sizeBytes: 0, sourceName: "tall.png", reclaimedAt: nil)
        }
        let first = attachment("first")
        let second = attachment("second")
        let bitmap = try XCTUnwrap(NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 390, pixelsHigh: 6245,
                                                   bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true,
                                                   isPlanar: false, colorSpaceName: .deviceRGB,
                                                   bytesPerRow: 0, bitsPerPixel: 0))
        let url = AttachmentBlobPaths.blobURL(for: first)
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try XCTUnwrap(bitmap.representation(using: .png, properties: [:])).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }
        let host = NSHostingView(rootView: AttachmentDetailView(attachment: first))
        host.frame = NSRect(x: 0, y: 0, width: 700, height: 500)
        func find(_ view: NSView) -> AttachmentZoomScrollView? {
            (view as? AttachmentZoomScrollView) ?? view.subviews.lazy.compactMap { find($0) }.first
        }
        host.layoutSubtreeIfNeeded()
        try await Task.sleep(for: .milliseconds(100))
        let original = try XCTUnwrap(find(host))
        original.apply(.width)
        XCTAssertEqual(original.mode, .width)
        host.rootView = AttachmentDetailView(attachment: second)
        host.layoutSubtreeIfNeeded()
        try await Task.sleep(for: .milliseconds(100))
        let replacement = try XCTUnwrap(find(host))
        XCTAssertFalse(original === replacement)
        XCTAssertEqual(replacement.mode, .fit)
        XCTAssertLessThan(replacement.magnification, 0.1)
        replacement.apply(.actual)
        host.rootView = AttachmentDetailView(attachment: first)
        host.layoutSubtreeIfNeeded()
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(try XCTUnwrap(find(host)).mode, .fit)
    }

    private func viewer(size: NSSize = NSSize(width: 390, height: 6245)) -> AttachmentZoomScrollView {
        let view = AttachmentZoomScrollView(image: NSImage(size: size))
        view.frame = NSRect(x: 0, y: 0, width: 700, height: 500)
        view.layoutSubtreeIfNeeded()
        view.apply(.fit)
        return view
    }

    func testTallImageFitWidthAndActualSize() {
        let view = viewer()
        XCTAssertEqual(view.magnification, 500 / 6245, accuracy: 0.001)
        XCTAssertEqual(view.picture.visibleRect.height, 6245, accuracy: 1)
        view.apply(.width)
        XCTAssertEqual(view.magnification, 700 / 390, accuracy: 0.001)
        XCTAssertEqual(view.picture.visibleRect.width, 390, accuracy: 1)
        XCTAssertEqual(view.picture.visibleRect.maxY, 6245, accuracy: 1)
        XCTAssertLessThan(view.picture.visibleRect.height, 300)
        view.apply(.actual)
        XCTAssertEqual(view.magnification, 1)
        view.apply(.in)
        XCTAssertEqual(view.magnification, 1.25)
        view.apply(.out)
        XCTAssertEqual(view.magnification, 1)
    }

    func testNormalImageKeepsAllEdgesVisibleAndCentered() {
        let view = viewer(size: NSSize(width: 1200, height: 800))
        XCTAssertEqual(view.magnification, 700 / 1200, accuracy: 0.001)
        let displayed = view.picture.convert(view.picture.bounds, to: view)
        XCTAssertEqual(displayed.width, 700, accuracy: 1)
        XCTAssertEqual(displayed.height, 800 * 700 / 1200, accuracy: 1)
        XCTAssertEqual(displayed.midX, view.contentView.frame.midX, accuracy: 1)
        XCTAssertEqual(displayed.midY, view.contentView.frame.midY, accuracy: 1)
        XCTAssertTrue(view.contentView.frame.insetBy(dx: -1, dy: -1).contains(displayed))
        XCTAssertLessThan(view.contentView.bounds.minY, 0)
    }

    func testFitDoesNotEnlargeSmallImages() {
        let view = viewer(size: NSSize(width: 120, height: 80))
        XCTAssertEqual(view.magnification, 1)
        let displayed = view.picture.convert(view.picture.bounds, to: view)
        XCTAssertEqual(displayed.size, NSSize(width: 120, height: 80))
        XCTAssertEqual(displayed.midX, view.contentView.frame.midX, accuracy: 1)
        XCTAssertEqual(displayed.midY, view.contentView.frame.midY, accuracy: 1)
    }

    func testFitWidthAccountsForLegacyScrollerGutters() {
        let view = viewer()
        view.scrollerStyle = .legacy
        view.apply(.width)
        view.layoutSubtreeIfNeeded()
        XCTAssertEqual(view.picture.visibleRect.width, 390, accuracy: 0.1)
        XCTAssertEqual(view.picture.visibleRect.maxY, 6245, accuracy: 0.1)
        XCTAssertTrue(view.horizontalScroller?.isHidden == true)
        XCTAssertTrue(view.verticalScroller?.isHidden == false)
        view.apply(.fit)
        view.layoutSubtreeIfNeeded()
        let displayed = view.picture.convert(view.picture.bounds, to: view)
        XCTAssertTrue(view.contentView.frame.insetBy(dx: -1, dy: -1).contains(displayed))
    }

    func testFitModesResizeAndDoubleClickToggles() {
        let view = viewer()
        view.apply(.width)
        view.frame.size.width = 600
        view.layoutSubtreeIfNeeded()
        XCTAssertEqual(view.magnification, 600 / 390, accuracy: 0.001)
        view.toggleFit(at: NSPoint(x: 195, y: 6000))
        XCTAssertEqual(view.mode, .fit)
        view.toggleFit(at: NSPoint(x: 195, y: 6000))
        XCTAssertEqual(view.magnification, 1)
        view.frame.size.height = 400
        view.layoutSubtreeIfNeeded()
        XCTAssertEqual(view.magnification, 1)
    }

    func testZoomKeepsInteriorAnchorAndScrollsBothAxes() {
        let view = viewer(size: NSSize(width: 2000, height: 6000))
        view.apply(.actual)
        let anchor = NSPoint(x: 1000, y: 3000)
        view.setMagnification(1, centeredAt: anchor)
        let before = view.picture.convert(anchor, to: view).x
        view.apply(.in, anchor: anchor)
        XCTAssertEqual(view.picture.convert(anchor, to: view).x, before, accuracy: 1)
        view.contentView.scroll(to: NSPoint(x: 400, y: 400))
        XCTAssertEqual(view.contentView.bounds.origin.x, 400, accuracy: 1)
        XCTAssertEqual(view.contentView.bounds.origin.y, 400, accuracy: 1)
        let down = NSEvent.mouseEvent(with: .leftMouseDown, location: NSPoint(x: 100, y: 100),
                                      modifierFlags: [], timestamp: 0, windowNumber: 0,
                                      context: nil, eventNumber: 0, clickCount: 1, pressure: 1)!
        let drag = NSEvent.mouseEvent(with: .leftMouseDragged, location: NSPoint(x: 50, y: 75),
                                      modifierFlags: [], timestamp: 1, windowNumber: 0,
                                      context: nil, eventNumber: 1, clickCount: 1, pressure: 1)!
        view.picture.mouseDown(with: down)
        view.picture.mouseDragged(with: drag)
        XCTAssertEqual(view.contentView.bounds.origin.x, 440, accuracy: 1)
        XCTAssertEqual(view.contentView.bounds.origin.y, 420, accuracy: 1)
        view.picture.mouseUp(with: drag)
    }
}
