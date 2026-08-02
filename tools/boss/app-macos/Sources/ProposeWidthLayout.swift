import SwiftUI

/// Proposes a fixed width to its single subview and then adopts **whatever
/// width the subview actually returns**, without clamping it back down.
///
/// This is the one shape SwiftUI's stock `frame` modifiers cannot express, and
/// a wide markdown table inside a horizontal scroll container needs exactly it:
///
/// - `.frame(maxWidth: w)` proposes `w` (so compressible cells wrap, good) but
///   clamps its *own* reported width to `w`. A `Grid` whose content cannot
///   compress below `w` still reports — and paints — wider than that, so the
///   enclosing `ScrollView` sizes its scroll extent to the clamped width and
///   the overflow is painted but unreachable. That is a hard cut with no
///   scrollbar.
/// - `.frame(minWidth: w)` only ever grows the box, so it never strands
///   content. But a `ScrollView(.horizontal)` proposes `nil` along its scroll
///   axis (measured, not assumed — see `MarkdownTableOverflowTests`), and a
///   `nil` proposal makes every `Text` report its full single-line width. Under
///   `minWidth` an ordinary prose cell therefore never wraps: it runs off the
///   viewport and the reader has to scroll horizontally to read a sentence.
///
/// Proposing the viewport width and reporting the subview's real size gets both
/// behaviours from one rule: content that *can* fit does fit (cells wrap, and
/// the table centers on the same axis as the surrounding prose), and content
/// that genuinely cannot fit reports its true width, so the scroll view gives
/// it a real scroll extent and every column stays reachable.
///
/// `width` is optional so callers can pass a not-yet-measured viewport through
/// unchanged; a `nil` width forwards the incoming proposal untouched.
struct ProposeWidthLayout: Layout {
    /// The width to propose to the subview, or `nil` to forward the incoming
    /// proposal unchanged.
    var width: CGFloat?

    func sizeThatFits(
        proposal: ProposedViewSize, subviews: Subviews, cache: inout ()
    ) -> CGSize {
        subviews.reduce(into: CGSize.zero) { result, subview in
            let size = subview.sizeThatFits(childProposal(proposal))
            result.width = max(result.width, size.width)
            result.height = max(result.height, size.height)
        }
    }

    func placeSubviews(
        in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()
    ) {
        for subview in subviews {
            subview.place(
                at: CGPoint(x: bounds.minX, y: bounds.minY),
                anchor: .topLeading,
                proposal: childProposal(proposal)
            )
        }
    }

    private func childProposal(_ proposal: ProposedViewSize) -> ProposedViewSize {
        ProposedViewSize(width: width ?? proposal.width, height: proposal.height)
    }
}
