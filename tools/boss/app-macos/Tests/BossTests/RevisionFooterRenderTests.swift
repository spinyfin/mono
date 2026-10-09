import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// Real card renders at the default 280-point column width, including its insets.
@MainActor
final class RevisionFooterRenderTests: XCTestCase {
    func testReviewCardsRender() throws {
        var cardHeights: [CGFloat] = []
        for inRevision in [false, true] {
            var task = WorkTask(
                id: "footer-fixture", productID: "fixture", projectID: nil, kind: "chore",
                name: "Review card footer", description: "", status: "in_review",
                priority: "medium", ordinal: nil,
                prURL: "https://github.com/spinyfin/mono/pull/3040",
                deletedAt: nil, createdAt: "", updatedAt: ""
            )
            task.shortID = 1234
            task.ciRequiredState = "in_progress"
            task.prMergeableState = "conflicting"
            task.hasInProgressRevision = inRevision
            let snapshot = WorkCardSnapshot.build(
                task: task, context: WorkCardSnapshotContext(column: .review)
            )
            let card = WorkBoardCardView(snapshot: snapshot, onRevisionBadgeTap: {})
                .frame(width: 252)
                .padding(14)
                .background(Color(nsColor: .windowBackgroundColor))
            let host = NSHostingView(rootView: card)
            host.appearance = NSAppearance(named: .aqua)
            host.frame = NSRect(origin: .zero, size: host.fittingSize)
            host.layoutSubtreeIfNeeded()
            cardHeights.append(host.bounds.height)
            let rep = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
            host.cacheDisplay(in: host.bounds, to: rep)
            XCTAssertGreaterThan(rep.pixelsHigh, 50)
            let data = try XCTUnwrap(rep.representation(using: .png, properties: [:]))
            if let output = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"] {
                let name = inRevision ? "with-revision.png" : "without-revision.png"
                try data.write(to: URL(fileURLWithPath: output).appendingPathComponent(name))
            }
        }
        XCTAssertGreaterThan(cardHeights[1] - cardHeights[0], 20,
                             "The revision badge must occupy its own row below the footer")
    }
}
