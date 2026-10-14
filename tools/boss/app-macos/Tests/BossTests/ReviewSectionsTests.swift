import XCTest
@testable import Boss

/// Coverage for the Review column's labelled sections. The engine classifies
/// each card (`Task.review_section`, see `classify_review_section`); the board
/// only groups by the key it is given. These tests pin that the column always
/// shows every card, in the fixed section order, with empty sections hidden
/// and each section's incoming card order preserved.
@MainActor
final class ReviewSectionsTests: XCTestCase {

    func testSectionsAppearInFixedOrderWithTheirCards() {
        let model = makeModel()
        model.choresByProductID = ["prod_test": [
            makeReviewItem(id: "blocked", section: .blocked),
            makeReviewItem(id: "pending", section: .pending),
            makeReviewItem(id: "ai", section: .aiReview),
            makeReviewItem(id: "revision", section: .inRevision),
            makeReviewItem(id: "ready", section: .ready),
        ]]

        let sections = model.workSections(in: .review)

        XCTAssertEqual(sections.map(\.title), ["Ready", "In Revision", "AI Review", "Pending", "Blocked"])
        XCTAssertEqual(
            sections.map { $0.items.map(\.id) },
            [["ready"], ["revision"], ["ai"], ["pending"], ["blocked"]]
        )
    }

    func testEmptySectionsAreHidden() {
        let model = makeModel()
        model.choresByProductID = ["prod_test": [
            makeReviewItem(id: "ready", section: .ready),
            makeReviewItem(id: "blocked", section: .blocked),
        ]]

        XCTAssertEqual(model.workSections(in: .review).map(\.title), ["Ready", "Blocked"])
    }

    func testEveryCardIsShownExactlyOnce() {
        let model = makeModel()
        let all = ReviewSection.allCases.enumerated().flatMap { index, section in
            [
                makeReviewItem(id: "a\(index)", section: section),
                makeReviewItem(id: "b\(index)", section: section),
            ]
        }
        model.choresByProductID = ["prod_test": all]

        let shown = model.workSections(in: .review).flatMap { $0.items.map(\.id) }
        XCTAssertEqual(shown.count, all.count)
        XCTAssertEqual(Set(shown), Set(all.map(\.id)))
        XCTAssertEqual(
            model.workItems(in: .review).count, all.count,
            "the column no longer filters: every Review card is in workItems"
        )
    }

    func testCardOrderWithinASectionIsPreserved() {
        let model = makeModel()
        // `reviewBoardSort` orders by creation time, newest first, so the
        // section must keep whatever order the column produced.
        var older = makeReviewItem(id: "older", section: .ready)
        older.createdAt = "2026-05-01T00:00:00Z"
        var newer = makeReviewItem(id: "newer", section: .ready)
        newer.createdAt = "2026-05-20T00:00:00Z"
        model.choresByProductID = ["prod_test": [older, newer]]

        let columnOrder = model.workItems(in: .review).map(\.id)
        let section = model.workSections(in: .review).first
        XCTAssertEqual(section?.items.map(\.id), columnOrder)
        XCTAssertEqual(columnOrder, ["newer", "older"])
    }

    func testCardWithoutAnEngineSectionLandsInPendingNotHidden() {
        let model = makeModel()
        model.choresByProductID = ["prod_test": [makeReviewItem(id: "unclassified", section: nil)]]

        let sections = model.workSections(in: .review)
        XCTAssertEqual(sections.map(\.title), ["Pending"])
        XCTAssertEqual(sections.first?.items.map(\.id), ["unclassified"])
    }

    func testReviewIsGroupedByStateEvenWhenBoardIsGroupedByProject() {
        let model = makeModel()
        model.workBoardGrouping = .project
        model.choresByProductID = ["prod_test": [
            makeReviewItem(id: "ready", section: .ready),
            makeReviewItem(id: "blocked", section: .blocked),
        ]]

        XCTAssertEqual(model.workSections(in: .review).map(\.title), ["Ready", "Blocked"])
    }

    func testSectionsAreCollapsibleAndDefaultExpanded() {
        let model = makeModel()
        model.choresByProductID = ["prod_test": [makeReviewItem(id: "ready", section: .ready)]]

        let section = model.workSections(in: .review).first
        XCTAssertEqual(section?.id, "review-ready")
        XCTAssertEqual(section?.isCollapsible, true)
        XCTAssertEqual(section?.defaultExpanded, true)
        XCTAssertNil(section?.groupKey, "a drop on a Review section names only the column")
    }

    func testOtherColumnsAreNotSectioned() {
        let model = makeModel()
        model.choresByProductID = ["prod_test": [makeBacklogTask(id: "backlog")]]

        XCTAssertEqual(model.workSections(in: .backlog).map(\.title), ["Backlog"])
    }

    func testReviewSectionWireValuesParse() {
        XCTAssertEqual(
            ["ready", "in_revision", "ai_review", "pending", "blocked"].compactMap(ReviewSection.init(rawValue:)),
            ReviewSection.allCases
        )
        XCTAssertNil(ReviewSection(rawValue: "something_new"))
    }

    // MARK: - Helpers

    private func makeReviewItem(id: String, section: ReviewSection?) -> WorkTask {
        var task = WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: id,
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/org/repo/pull/1",
            deletedAt: nil,
            createdAt: "2026-05-15T00:00:00Z",
            updatedAt: "2026-05-15T00:00:00Z",
            autostart: true
        )
        task.reviewSection = section
        return task
    }

    private func makeBacklogTask(id: String) -> WorkTask {
        // `autostart: false` so a `"todo"` status task lands in Backlog, not
        // Doing (`WorkTask.boardColumn` routes `"todo"` + `autostart` to Doing).
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: id,
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-15T00:00:00Z",
            updatedAt: "2026-05-15T00:00:00Z",
            autostart: false
        )
    }

    private func makeModel() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.products = [
            WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-13T00:00:00Z",
                updatedAt: "2026-05-13T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = "prod_test"
        return model
    }
}
