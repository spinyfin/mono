import AppKit
import SwiftUI
import XCTest
@testable import Boss

private struct BadgeFrames: PreferenceKey {
    static let defaultValue: [Int: CGRect] = [:]
    static func reduce(value: inout [Int: CGRect], nextValue: () -> [Int: CGRect]) {
        value.merge(nextValue(), uniquingKeysWith: { _, new in new })
    }
}

/// Observe the constraint independently of Text's glyph-fitted rendered width.
private struct BadgeProposalProbe: Layout {
    var record: (CGFloat) -> Void

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        subviews[0].sizeThatFits(proposal)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        record(proposal.width ?? .infinity)
        subviews[0].place(at: bounds.origin, anchor: .topLeading, proposal: proposal)
    }
}

@MainActor
final class BadgeFlowLayoutTests: XCTestCase {
    private let longLabel = String(repeating: "Very long badge label ", count: 12)

    private func measured<V: View>(
        _ view: V, id: Int, record: @escaping (CGFloat) -> Void = { _ in }
    ) -> some View {
        BadgeProposalProbe(record: record) { view }.background(GeometryReader { proxy in
            Color.clear.preference(key: BadgeFrames.self, value: [id: proxy.frame(in: .named("flow"))])
        })
    }

    func testBadgesStayInsideContentWidthAndResize() throws {
        var frames: [Int: CGRect] = [:]
        var proposals: [Int: CGFloat] = [:]
        let flow = FlowLayout(horizontalSpacing: 6, verticalSpacing: 4) {
            measured(WorkStatusBadge(text: "Short"), id: 0)
            measured(WorkStatusBadge(text: longLabel, hasMoreInfo: true), id: 1) { proposals[1] = $0 }
            measured(RepoChipView(presentation: RepoChipPresentation(
                shortName: longLabel, fullURL: "https://github.com/example/repo", provenance: .taskOverride
            )), id: 2) { proposals[2] = $0 }
            measured(WorkTagChip(text: longLabel), id: 3) { proposals[3] = $0 }
            measured(WorkStatusBadge(text: "Last"), id: 4)
            measured(ExternalRefLinkView(presentation: ExternalRefLinkPresentation(
                label: longLabel, url: "https://example.com/issue", tooltip: longLabel, isStale: false
            )), id: 5)
            measured(AIReviewStateBadge(state: "reviewing", presentation: AIReviewBadgePresentation(
                label: longLabel, systemImage: "checkmark", tooltip: longLabel
            )), id: 6)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .coordinateSpace(name: "flow")
        .onPreferenceChange(BadgeFrames.self) { frames = $0 }
        let host = NSHostingView(rootView: flow)
        var shortSize: CGSize?
        // Lane widths minus 28pt lane padding and 24pt card padding.
        // Reuse the host to exercise cache invalidation in both directions.
        for width in [CGFloat(288), 228, 368, 228] {
            host.frame = NSRect(x: 0, y: 0, width: width, height: 300)
            host.layoutSubtreeIfNeeded()
            let rep = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
            host.cacheDisplay(in: host.bounds, to: rep)
            XCTAssertEqual(frames.count, 7)
            for frame in frames.values {
                XCTAssertGreaterThan(frame.width, 0)
                XCTAssertGreaterThanOrEqual(frame.minX, -0.5)
                XCTAssertLessThanOrEqual(frame.maxX, width + 0.5)
                XCTAssertLessThan(frame.height, 30, "Badge labels must not wrap")
            }
            let short = try XCTUnwrap(frames[0])
            if let shortSize {
                XCTAssertEqual(short.width, shortSize.width, accuracy: 0.5)
                XCTAssertEqual(short.height, shortSize.height, accuracy: 0.5)
            } else {
                shortSize = short.size
            }
            XCTAssertLessThan(short.width, 100, "Short chips must retain their natural width")
            for id in 1...3 {
                let frame = try XCTUnwrap(frames[id])
                XCTAssertEqual(try XCTUnwrap(proposals[id]), width, accuracy: 0.5,
                               "Oversized badges must receive the full available width")
                XCTAssertLessThanOrEqual(frame.height, short.height + 1, "Labels must stay on one line")
                let previous = try XCTUnwrap(frames[id - 1])
                XCTAssertGreaterThanOrEqual(frame.minY, previous.maxY + 3.5, "Wrapped rows must not overlap")
            }
        }
    }
}
