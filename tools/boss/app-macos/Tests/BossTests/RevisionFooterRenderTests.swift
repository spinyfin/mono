import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// Real card renders at the default 280-point column width, including its insets.
@MainActor
final class RevisionFooterRenderTests: XCTestCase {
    private struct Fixture {
        let name: String
        let inRevision: Bool
        let review: Bool
        let rollups: Bool
    }

    private func render(_ fixture: Fixture) throws -> (height: CGFloat, idFrame: CGRect?, png: Data) {
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
        task.hasInProgressRevision = fixture.inRevision
        if fixture.review {
            task.reviewRequiredState = "required"
        }
        var context = WorkCardSnapshotContext(column: .review)
        if fixture.rollups {
            context.inReviewRevisions = [
                WorkCardRevisionRollup(
                    id: "rev-1", revisionSeq: 1, name: "Revision one",
                    revisionParentPrUrl: nil
                ),
            ]
        }
        let snapshot = WorkCardSnapshot.build(task: task, context: context)
        var idFrame: CGRect?
        let card = WorkBoardCardView(snapshot: snapshot, onRevisionBadgeTap: {})
            .frame(width: 252)
            .padding(14)
            .background(Color(nsColor: .windowBackgroundColor))
            .onPreferenceChange(ShortIDFramePreferenceKey.self) { idFrame = $0 }
        let host = NSHostingView(rootView: card)
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(origin: .zero, size: host.fittingSize)
        host.layoutSubtreeIfNeeded()
        let rep = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
        host.cacheDisplay(in: host.bounds, to: rep)
        XCTAssertGreaterThan(rep.pixelsHigh, 50)
        let data = try XCTUnwrap(rep.representation(using: .png, properties: [:]))
        if let output = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"] {
            try data.write(to: URL(fileURLWithPath: output).appendingPathComponent(fixture.name + ".png"))
        }
        return (host.bounds.height, idFrame, data)
    }

    func testReviewCardsRender() throws {
        let plain = try render(Fixture(name: "without-revision", inRevision: false, review: false, rollups: false))
        let badge = try render(Fixture(name: "with-revision", inRevision: true, review: false, rollups: false))
        XCTAssertGreaterThan(badge.height - plain.height, 20,
                             "The revision badge must occupy its own row below the footer")
    }

    func testShortIDStaysBottomRight() throws {
        let width: CGFloat = 252 + 28
        for inRevision in [false, true] {
            for review in [false, true] {
                for rollups in [false, true] {
                    let fixture = Fixture(
                        name: "anchor-\(inRevision)-\(review)-\(rollups)",
                        inRevision: inRevision, review: review, rollups: rollups
                    )
                    let result = try render(fixture)
                    let frame = try XCTUnwrap(result.idFrame, fixture.name)
                    // Card insets: 14pt padding here plus the card's own inner padding.
                    XCTAssertGreaterThan(frame.maxX, width - 50, "\(fixture.name): id not at trailing edge")
                    XCTAssertGreaterThan(frame.maxY, result.height - 50, "\(fixture.name): id not in bottom row")
                }
            }
        }
    }
}
