import XCTest
@testable import Boss

/// Coverage for the Review column "ready to merge" filter: `readyForReview`
/// is an engine-computed fact (`Task.ready_for_review`) that the view only
/// reads, never re-derives from badge state. `ChatViewModel.reviewReadyOnly`
/// is the view-only toggle that narrows `workItems(in: .review)` down to
/// those cards, and group headers (project groups, the "Chores (N)" group)
/// must reflect the filtered counts — a group left with zero ready cards
/// must not render an empty header.
@MainActor
final class ReviewReadyFilterTests: XCTestCase {

    func testReviewReadyOnlyOffShowsEveryReviewCard() {
        let model = makeModel()
        let ready = makeReviewItem(id: "task_ready", name: "Ready", readyForReview: true)
        let notReady = makeReviewItem(id: "task_not_ready", name: "Not ready", readyForReview: false)
        model.choresByProductID = ["prod_test": [ready, notReady]]

        XCTAssertFalse(model.reviewReadyOnly, "filter defaults to off")
        XCTAssertEqual(Set(model.workItems(in: .review).map(\.id)), ["task_ready", "task_not_ready"])
    }

    func testReviewReadyOnlyHidesNotReadyCards() {
        let model = makeModel()
        let ready = makeReviewItem(id: "task_ready", name: "Ready", readyForReview: true)
        let notReady = makeReviewItem(id: "task_not_ready", name: "Not ready", readyForReview: false)
        model.choresByProductID = ["prod_test": [ready, notReady]]

        model.setReviewReadyOnly(true)

        XCTAssertEqual(model.workItems(in: .review).map(\.id), ["task_ready"])
    }

    func testReviewReadyOnlyDoesNotAffectOtherColumns() {
        let model = makeModel()
        let backlogTask = makeTask(id: "task_backlog", name: "Backlog", status: "todo")
        model.choresByProductID = ["prod_test": [backlogTask]]

        model.setReviewReadyOnly(true)

        XCTAssertEqual(
            model.workItems(in: .backlog).map(\.id), ["task_backlog"],
            "the ready-only filter is scoped to the Review column only"
        )
    }

    func testReviewReadyOnlyIsAViewToggleThatDoesNotMutateTasks() {
        let model = makeModel()
        let notReady = makeReviewItem(id: "task_not_ready", name: "Not ready", readyForReview: false)
        model.choresByProductID = ["prod_test": [notReady]]

        model.setReviewReadyOnly(true)
        XCTAssertTrue(model.workItems(in: .review).isEmpty)

        // Toggling back off must restore the card exactly as it was —
        // the filter never wrote to the task itself.
        model.setReviewReadyOnly(false)
        let restored = model.workItems(in: .review)
        XCTAssertEqual(restored.map(\.id), ["task_not_ready"])
        XCTAssertEqual(restored.first?.readyForReview, false)
    }

    func testGroupWithNoReadyCardsIsOmittedWhenFilterIsOn() {
        let model = makeModel()
        model.workBoardGrouping = .project
        let readyChore = makeReviewItem(id: "task_ready_chore", name: "Ready chore", readyForReview: true)
        var notReadyProjectTask = makeReviewItem(id: "task_not_ready_proj", name: "Not ready", readyForReview: false)
        notReadyProjectTask = WorkTask(
            id: notReadyProjectTask.id,
            productID: notReadyProjectTask.productID,
            projectID: "proj_test",
            kind: "project_task",
            name: notReadyProjectTask.name,
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/org/repo/pull/2",
            deletedAt: nil,
            createdAt: "2026-05-15T00:00:00Z",
            updatedAt: "2026-05-15T00:00:00Z",
            autostart: true,
            readyForReview: false
        )
        model.choresByProductID = ["prod_test": [readyChore]]
        model.productLevelTasksByProductID = ["prod_test": [notReadyProjectTask]]
        model.projectsByProductID = [
            "prod_test": [
                WorkProject(
                    id: "proj_test",
                    productID: "prod_test",
                    name: "Test Project",
                    slug: "test-project",
                    description: "",
                    goal: "",
                    status: "active",
                    priority: "medium",
                    createdAt: "2026-05-01T00:00:00Z",
                    updatedAt: "2026-05-01T00:00:00Z"
                )
            ]
        ]

        model.setReviewReadyOnly(true)

        let sections = model.workSections(in: .review)
        XCTAssertEqual(sections.map(\.title), ["Chores"], "the empty 'Test Project' group must not render a header")
        XCTAssertEqual(sections.first?.items.map(\.id), ["task_ready_chore"])
    }

    // MARK: - Helpers

    private func makeReviewItem(id: String, name: String, readyForReview: Bool) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: name,
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/org/repo/pull/1",
            deletedAt: nil,
            createdAt: "2026-05-15T00:00:00Z",
            updatedAt: "2026-05-15T00:00:00Z",
            autostart: true,
            readyForReview: readyForReview
        )
    }

    private func makeTask(id: String, name: String, status: String) -> WorkTask {
        // `autostart: false` so a `"todo"` status task lands in Backlog, not
        // Doing (`WorkTask.boardColumn` routes `"todo"` + `autostart` to Doing).
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: name,
            description: "",
            status: status,
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
