import SwiftUI

/// The vertical scroll view of one kanban column.
///
/// The column anchors its scroll position to a *card*, not to a pixel
/// offset. Review's Pending section is ordered by most recent activity, so a
/// work-tree refresh can move the card the user is looking at — including the
/// one a reveal just centred — to another row; lazy rows above the viewport
/// also change height as they materialise. A plain `ScrollView` keeps the
/// offset and lets the content slide underneath, which is how a revealed card
/// vanished a moment after reveal. `scrollPosition(id:anchor:)` instead keeps
/// the card at the viewport centre in place across those relayouts. The
/// binding follows the user's own scrolling (SwiftUI updates it to whichever
/// card is at the centre), so it never fights them and a reveal "sticks" only
/// until they scroll away.
///
/// The one place the column deliberately does not anchor is the very top: a
/// user parked there is watching for cards that float up on activity, and
/// anchoring would scroll the column down to hide exactly those. See
/// `WorkBoardColumnAnchor.retained`.
///
/// The reveal scroll lives here too: it targets the card by id through the
/// same anchor, retrying while disclosure expansion and lazy layout settle,
/// until the card's viewport probe (`RevealCardViewport`) acknowledges it.
struct WorkBoardColumnScrollView<Content: View>: View {
    let column: WorkBoardColumnKey
    /// Every card id rendered in this column, in display order.
    let cardIDs: [String]
    let revealTarget: String?
    let revealGeneration: UUID
    let boardProxy: ScrollViewProxy
    /// Reads the live reveal target so the retry loop stops as soon as the
    /// reveal is confirmed or superseded.
    let isRevealStillPending: (String) -> Bool
    @ViewBuilder let content: () -> Content

    @State private var anchoredCardID: String?
    @State private var isAtTop = true

    var body: some View {
        ScrollView(.vertical) {
            VStack(alignment: .leading, spacing: 12) {
                content()
            }
            .frame(maxWidth: .infinity, alignment: .topLeading)
        }
        .scrollPosition(id: $anchoredCardID, anchor: .center)
        .onScrollGeometryChange(for: Bool.self) { geometry in
            geometry.contentOffset.y <= 1
        } action: { _, atTop in
            isAtTop = atTop
        }
        .frame(maxHeight: .infinity)
        // Re-evaluate eagerly on every input, not only when the cards change:
        // by the time a reorder reaches `onChange(of: cardIDs)` SwiftUI has
        // already laid the new content out around whatever anchor was set.
        .onChange(of: cardIDs) { _, _ in retainAnchor() }
        .onChange(of: anchoredCardID) { _, _ in retainAnchor() }
        .onChange(of: isAtTop) { _, _ in retainAnchor() }
        .onChange(of: revealTarget) { _, _ in retainAnchor() }
        .task(id: revealTarget.map { "\(revealGeneration)-\($0)" }) {
            guard let target = WorkBoardColumnAnchor.revealAnchor(for: revealTarget, in: cardIDs) else { return }
            // Wait for disclosure expansion and lazy card layout. Retry
            // until the card's viewport probe acknowledges this request.
            for _ in 0..<40 {
                guard !Task.isCancelled, isRevealStillPending(target) else { return }
                boardProxy.scrollTo(column, anchor: .center)
                anchoredCardID = target
                do { try await Task.sleep(for: .milliseconds(50)) } catch { return }
            }
        }
    }

    private func retainAnchor() {
        let retained = WorkBoardColumnAnchor.retained(
            anchoredCardID, in: cardIDs, atTop: isAtTop, revealTarget: revealTarget
        )
        if retained != anchoredCardID {
            anchoredCardID = retained
        }
    }
}

/// Pure anchor bookkeeping for `WorkBoardColumnScrollView`, kept free of
/// SwiftUI so it can be unit-tested.
enum WorkBoardColumnAnchor {
    /// The card the column keeps in place across its next relayout, or `nil`
    /// to fall back to the plain pixel offset:
    ///
    /// - the anchored card must still be one of the column's cards — a card
    ///   that moved to another column must not be chased there;
    /// - a column scrolled to its top keeps no anchor unless a reveal is
    ///   pending, so a card that floats up on activity appears at the top
    ///   instead of being scrolled out of view above the anchored card.
    static func retained(
        _ anchoredCardID: String?, in cardIDs: [String], atTop: Bool, revealTarget: String?
    ) -> String? {
        guard let anchoredCardID, cardIDs.contains(anchoredCardID) else { return nil }
        if atTop, revealTarget == nil { return nil }
        return anchoredCardID
    }

    /// The card a reveal should anchor this column on: the target when it is
    /// one of this column's cards, otherwise none (another column owns it).
    static func revealAnchor(for revealTarget: String?, in cardIDs: [String]) -> String? {
        guard let revealTarget, cardIDs.contains(revealTarget) else { return nil }
        return revealTarget
    }
}
