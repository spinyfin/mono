import SwiftUI

/// The vertical scroll view of one kanban column.
///
/// The column anchors its scroll position to a *card*, not to a pixel
/// offset. Review's Pending section is ordered by most recent activity, so a
/// work-tree refresh can move the card the user is looking at — including the
/// one a reveal just centred — to another row, and lazy rows above the
/// viewport change height as they materialise. A plain `ScrollView` keeps a
/// pixel offset, so that content slides out from under whatever the user or a
/// reveal was looking at; `scrollPosition(id:anchor:)` keeps the card at the
/// viewport centre in place across those relayouts. The binding follows the
/// user's own scrolling (SwiftUI updates it to whichever card is at the
/// centre), so it never fights them.
///
/// The one place the column deliberately does not anchor is the very top: a
/// user parked there is watching for cards that float up on activity, and
/// anchoring would scroll the column down to hide exactly those. A revealed
/// card is the exception: its anchor is *pinned* from the moment the reveal
/// starts until the user scrolls, the card leaves the column, or another
/// reveal replaces it, so a refresh that moves it to offset 0 does not drop
/// it. See `WorkBoardColumnAnchorState`.
///
/// The reveal scroll lives here too: it targets the card by id through the
/// vertical scroll proxy and the same anchor, retrying while disclosure
/// expansion and lazy layout settle, until the card's viewport probe
/// (`RevealCardViewport`) acknowledges it.
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

    @State private var anchor = WorkBoardColumnAnchorState()

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView(.vertical) {
                VStack(alignment: .leading, spacing: 12) {
                    content()
                }
                .frame(maxWidth: .infinity, alignment: .topLeading)
            }
            .scrollPosition(id: $anchor.cardID, anchor: .center)
            .onScrollGeometryChange(for: Bool.self) { geometry in
                geometry.contentOffset.y <= 1
            } action: { _, atTop in
                anchor.isAtTop = atTop
            }
            .onScrollPhaseChange { _, phase in
                if phase == .interacting { anchor.userScrolled() }
            }
            .frame(maxHeight: .infinity)
            // Re-evaluate eagerly on every input, not only when the cards
            // change: by the time a reorder reaches `onChange(of: cardIDs)`
            // SwiftUI has already laid the new content out around whatever
            // anchor was set.
            .onChange(of: cardIDs) { _, _ in anchor.reconcile(in: cardIDs) }
            .onChange(of: anchor) { _, _ in anchor.reconcile(in: cardIDs) }
            .task(id: revealTarget.map { "\(revealGeneration)-\($0)" }) {
                guard let target = WorkBoardColumnAnchor.revealAnchor(for: revealTarget, in: cardIDs) else { return }
                anchor.beginReveal(on: target)
                // Wait for disclosure expansion and lazy card layout. Each
                // pass re-issues the scroll — the binding alone only scrolls
                // when its value changes — until the card's viewport probe
                // acknowledges this request.
                for _ in 0..<40 {
                    guard !Task.isCancelled, isRevealStillPending(target) else { return }
                    boardProxy.scrollTo(column, anchor: .center)
                    proxy.scrollTo(target, anchor: .center)
                    do { try await Task.sleep(for: .milliseconds(50)) } catch { return }
                }
            }
        }
    }
}

/// The anchor a column keeps across relayouts, kept free of SwiftUI so its
/// transitions can be unit-tested.
struct WorkBoardColumnAnchorState: Equatable {
    var cardID: String?
    var isAtTop = true
    /// Set by a reveal; cleared only by the user scrolling, the card leaving
    /// the column, or the pin being re-established by a new reveal.
    var isRevealPinned = false

    mutating func beginReveal(on target: String) {
        cardID = target
        isRevealPinned = true
    }

    mutating func userScrolled() {
        isRevealPinned = false
    }

    mutating func reconcile(in cardIDs: [String]) {
        let retained = WorkBoardColumnAnchor.retained(
            cardID, in: cardIDs, atTop: isAtTop, pinned: isRevealPinned
        )
        if retained != cardID { cardID = retained }
        if retained == nil { isRevealPinned = false }
    }
}

/// Pure anchor rules for `WorkBoardColumnScrollView`.
enum WorkBoardColumnAnchor {
    /// The card the column keeps in place across its next relayout, or `nil`
    /// to fall back to the plain pixel offset:
    ///
    /// - the anchored card must still be one of the column's cards — a card
    ///   that moved to another column must not be chased there;
    /// - a column scrolled to its top keeps no anchor unless a reveal has it
    ///   pinned, so a card that floats up on activity appears at the top
    ///   instead of being scrolled out of view above the anchored card.
    static func retained(
        _ anchoredCardID: String?, in cardIDs: [String], atTop: Bool, pinned: Bool
    ) -> String? {
        guard let anchoredCardID, cardIDs.contains(anchoredCardID) else { return nil }
        if atTop, !pinned { return nil }
        return anchoredCardID
    }

    /// The card a reveal should anchor this column on: the target when it is
    /// one of this column's cards, otherwise none (another column owns it).
    static func revealAnchor(for revealTarget: String?, in cardIDs: [String]) -> String? {
        guard let revealTarget, cardIDs.contains(revealTarget) else { return nil }
        return revealTarget
    }
}
