import XCTest
@testable import Boss

/// `WorkBoardColumnScrollView` keeps a kanban column anchored on the card at
/// the viewport centre so a refresh that reorders rows (Review's Pending
/// section is ordered by most recent activity) or resizes lazy rows does not
/// slide the content out from under the user — or out from under a card that
/// reveal just centred. These pin the pure anchor rules the view applies.
final class WorkBoardColumnAnchorTests: XCTestCase {
    private let cards = ["card-a", "card-b", "card-c"]

    func testAnchorIsKeptWhileItsCardIsStillInTheColumn() {
        XCTAssertEqual(
            WorkBoardColumnAnchor.retained("card-b", in: cards, atTop: false, revealTarget: nil),
            "card-b"
        )
        // Reordering the column does not drop the anchor: that is the case
        // anchoring exists for.
        XCTAssertEqual(
            WorkBoardColumnAnchor.retained("card-b", in: cards.reversed(), atTop: false, revealTarget: nil),
            "card-b"
        )
    }

    func testAnchorIsDroppedWhenItsCardLeavesTheColumn() {
        XCTAssertNil(WorkBoardColumnAnchor.retained("card-b", in: ["card-a", "card-c"], atTop: false, revealTarget: nil))
        XCTAssertNil(WorkBoardColumnAnchor.retained("card-b", in: [], atTop: false, revealTarget: nil))
        XCTAssertNil(WorkBoardColumnAnchor.retained(nil, in: cards, atTop: false, revealTarget: nil))
    }

    func testColumnScrolledToTopKeepsNoAnchorSoFloatingCardsAppearThere() {
        // A user parked at the top is watching for cards that float up on
        // activity; anchoring would scroll the column down to hide them.
        XCTAssertNil(WorkBoardColumnAnchor.retained("card-a", in: cards, atTop: true, revealTarget: nil))
    }

    func testPendingRevealKeepsItsAnchorEvenAtTheTop() {
        // The reveal retry loop writes the target as the anchor until the
        // card's viewport probe confirms it; the top-of-column rule must not
        // fight that loop while the reveal is still pending.
        XCTAssertEqual(
            WorkBoardColumnAnchor.retained("card-a", in: cards, atTop: true, revealTarget: "card-a"),
            "card-a"
        )
    }

    func testRevealAnchorsOnlyTheColumnThatRendersTheTarget() {
        XCTAssertEqual(WorkBoardColumnAnchor.revealAnchor(for: "card-c", in: cards), "card-c")
        XCTAssertNil(WorkBoardColumnAnchor.revealAnchor(for: "card-z", in: cards))
        XCTAssertNil(WorkBoardColumnAnchor.revealAnchor(for: nil, in: cards))
    }
}
