import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class CriticalPriorityBannerTests: XCTestCase {
    private func request() throws -> CriticalPriorityRequest {
        try XCTUnwrap(CriticalPriorityRequest([
            "id": "request", "ask": "Please set the release fixes to critical.",
            "expires_at": Date().addingTimeInterval(300).timeIntervalSince1970,
            "items": [["id": "release-fix", "name": "Repair release packaging"]]
        ]))
    }

    func testRequestsAppearAndResolutionRemovesOnlyTheConfirmedBatch() throws {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let request = try request()
        model.applyEventForTest(.criticalPriorityRequests([request]))
        XCTAssertEqual(model.criticalPriorityRequests.map(\.id), ["request"])
        model.applyEventForTest(.criticalPriorityResolved("unknown"))
        XCTAssertEqual(model.criticalPriorityRequests.count, 1)
        model.applyEventForTest(.criticalPriorityResolved("request"))
        XCTAssertTrue(model.criticalPriorityRequests.isEmpty)
    }

    func testMalformedItemsAreNotSilentlyOmittedFromConfirmation() {
        XCTAssertNil(CriticalPriorityRequest([
            "id": "request", "ask": "critical", "expires_at": 123.0,
            "items": [["name": "missing identity"]]
        ]))
    }

    func testBannerRendersWithoutCreatingAWindow() throws {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.criticalPriorityRequests = [try request()]
        let host = NSHostingView(rootView: CriticalPriorityBanner(model: model))
        host.frame = NSRect(x: 0, y: 0, width: 850, height: 250)
        host.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(host.fittingSize.height, 0)
        XCTAssertNil(host.window)
        if let output = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"] {
            let image = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
            host.cacheDisplay(in: host.bounds, to: image)
            let png = try XCTUnwrap(image.representation(using: .png, properties: [:]))
            try png.write(to: URL(fileURLWithPath: output).appendingPathComponent("critical-confirmation.png"))
        }
    }
}
