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
            WorkBoardColumnAnchor.retained("card-b", in: cards, atTop: false, pinned: false),
            "card-b"
        )
        // Reordering the column does not drop the anchor: that is the case
        // anchoring exists for.
        XCTAssertEqual(
            WorkBoardColumnAnchor.retained("card-b", in: cards.reversed(), atTop: false, pinned: false),
            "card-b"
        )
    }

    func testAnchorIsDroppedWhenItsCardLeavesTheColumn() {
        XCTAssertNil(WorkBoardColumnAnchor.retained("card-b", in: ["card-a", "card-c"], atTop: false, pinned: false))
        XCTAssertNil(WorkBoardColumnAnchor.retained("card-b", in: [], atTop: false, pinned: false))
        XCTAssertNil(WorkBoardColumnAnchor.retained(nil, in: cards, atTop: false, pinned: false))
    }

    func testColumnScrolledToTopKeepsNoAnchorSoFloatingCardsAppearThere() {
        // A user parked at the top is watching for cards that float up on
        // activity; anchoring would scroll the column down to hide them.
        XCTAssertNil(WorkBoardColumnAnchor.retained("card-a", in: cards, atTop: true, pinned: false))
    }

    func testPendingRevealKeepsItsAnchorEvenAtTheTop() {
        // The reveal retry loop writes the target as the anchor until the
        // card's viewport probe confirms it; the top-of-column rule must not
        // fight that loop while the reveal is still pending.
        XCTAssertEqual(
            WorkBoardColumnAnchor.retained("card-a", in: cards, atTop: true, pinned: true),
            "card-a"
        )
    }

    func testRevealAnchorsOnlyTheColumnThatRendersTheTarget() {
        XCTAssertEqual(WorkBoardColumnAnchor.revealAnchor(for: "card-c", in: cards), "card-c")
        XCTAssertNil(WorkBoardColumnAnchor.revealAnchor(for: "card-z", in: cards))
        XCTAssertNil(WorkBoardColumnAnchor.revealAnchor(for: nil, in: cards))
    }

    func testConfirmedRevealStaysAnchoredWhenARefreshMovesItToTheTop() {
        var state = WorkBoardColumnAnchorState()
        state.isAtTop = false
        state.beginReveal(on: "card-c")
        // Reveal confirmed: the model clears its target, which must not
        // release the pin.
        state.reconcile(in: cards)
        XCTAssertEqual(state.cardID, "card-c")

        // A refresh moves the revealed card to row 0 and the column lands
        // at offset 0 with no user input.
        let refreshed = ["card-c", "card-a", "card-b"]
        state.isAtTop = true
        state.reconcile(in: refreshed)
        XCTAssertEqual(state.cardID, "card-c")

        // Other cards then float ahead of it; it is still the anchor.
        state.reconcile(in: ["card-x", "card-y", "card-c", "card-a", "card-b"])
        XCTAssertEqual(state.cardID, "card-c")
    }

    func testUserScrollReleasesTheRevealPin() {
        var state = WorkBoardColumnAnchorState()
        state.beginReveal(on: "card-b")
        state.userScrolled()
        state.isAtTop = true
        state.reconcile(in: cards)
        XCTAssertNil(state.cardID)
        XCTAssertFalse(state.isRevealPinned)
    }

    func testRevealPinEndsWhenItsCardLeavesTheColumn() {
        var state = WorkBoardColumnAnchorState()
        state.beginReveal(on: "card-b")
        state.reconcile(in: ["card-a", "card-c"])
        XCTAssertNil(state.cardID)
        XCTAssertFalse(state.isRevealPinned)
    }

    func testNewRevealReplacesThePreviousPin() {
        var state = WorkBoardColumnAnchorState()
        state.beginReveal(on: "card-a")
        state.beginReveal(on: "card-c")
        state.isAtTop = true
        state.reconcile(in: cards)
        XCTAssertEqual(state.cardID, "card-c")
    }

    func testCaptureFindsTheTargetByIdInATrimmedFixture() {
        func row(_ id: String) -> WorkTask {
            WorkTask(
                id: id, productID: "p", projectID: nil, kind: "chore", name: id,
                description: "", status: "todo", priority: "medium", ordinal: 0,
                prURL: nil, deletedAt: nil, createdAt: "", updatedAt: ""
            )
        }
        // The queued fixture keeps two rows, so the original target index
        // (29 of 30) is out of range.
        let trimmed = [row("reveal-row-28"), row("reveal-row-29")]
        XCTAssertEqual(BossRevealCapture.index(ofCard: "reveal-row-29", in: trimmed), 1)
        XCTAssertNil(BossRevealCapture.index(ofCard: "reveal-row-3", in: trimmed))
    }
}
