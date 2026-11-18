import AppKit
import XCTest
@testable import Boss

final class AttachmentImageSizingTests: XCTestCase {
    func testPortraitFitsHeightAndLandscapeFitsWidth() {
        XCTAssertEqual(AttachmentImageSizing.fit(
            native: CGSize(width: 1170, height: 2532), available: CGSize(width: 700, height: 600)
        ).height, 600, accuracy: 0.001)
        XCTAssertEqual(AttachmentImageSizing.fit(
            native: CGSize(width: 2400, height: 1200), available: CGSize(width: 700, height: 600)
        ), CGSize(width: 700, height: 350))
    }

    func testSmallImageIsNeverEnlarged() {
        XCTAssertEqual(AttachmentImageSizing.fit(
            native: CGSize(width: 120, height: 80), available: CGSize(width: 700, height: 600)
        ), CGSize(width: 120, height: 80))
    }

    func testResizingAndSidebarWidthsPreserveAspectAndBounds() {
        let native = CGSize(width: 1170, height: 2532)
        for width in [CGFloat(240), 440, 720, 1000] {
            for height in [CGFloat(200), 500, 900] {
                let fitted = AttachmentImageSizing.fit(native: native, available: CGSize(width: width, height: height))
                XCTAssertLessThanOrEqual(fitted.width, width)
                XCTAssertLessThanOrEqual(fitted.height, height)
                XCTAssertEqual(fitted.width / fitted.height, native.width / native.height, accuracy: 0.001)
            }
        }
    }

    func testEmptyLayoutOrImageDoesNotProduceInvalidDimensions() {
        XCTAssertEqual(AttachmentImageSizing.fit(native: .zero, available: CGSize(width: 700, height: 600)), .zero)
        XCTAssertEqual(AttachmentImageSizing.fit(native: CGSize(width: 120, height: 80), available: .zero), .zero)
    }
}
