import XCTest
@testable import Boss

/// Regression coverage for issue #1249: `bossctl reveal <id>` scrolls the
/// kanban to a card and highlights it, but if an active board filter
/// excludes the target the card stays hidden and the scroll lands on
/// nothing — `reveal` reports success while revealing nothing.
///
/// `revealWorkCard` must reset every narrowing filter (search query,
/// blocked-only, chores-only, hidden chores, project filter) before
/// scrolling so the revealed card is guaranteed visible.
@MainActor
final class RevealClearsFiltersTests: XCTestCase {

    func testRevealClearsSearchTextThatHidesTarget() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeTask(id: "task_a", name: "Apple"),
            makeTask(id: "task_b", name: "Banana"),
        ]))
        // A stale search filter that excludes the reveal target.
        model.workSearchText = "Banana"
        XCTAssertFalse(
            model.visibleWorkItems.contains { $0.id == "task_a" },
            "precondition: search filter hides the target card"
        )

        model.revealWorkCard("task_a", productID: "prod_test")

        XCTAssertEqual(model.workSearchText, "", "reveal must clear the search filter")
        XCTAssertTrue(
            model.visibleWorkItems.contains { $0.id == "task_a" },
            "revealed card must be visible after the search filter is cleared"
        )
        XCTAssertEqual(model.revealScrollTarget, "task_a", "reveal must queue a scroll to the card")
    }

    func testRevealClearsBlockedOnlyFilter() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeTask(id: "task_a", name: "Apple", status: "todo"),
        ]))
        model.showBlockedOnly = true
        XCTAssertFalse(model.visibleWorkItems.contains { $0.id == "task_a" })

        model.revealWorkCard("task_a", productID: "prod_test")

        XCTAssertFalse(model.showBlockedOnly, "reveal must clear the blocked-only filter")
        XCTAssertTrue(model.visibleWorkItems.contains { $0.id == "task_a" })
    }

    func testRevealClearsChoresOnlyFilterToShowATask() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(
            tasks: [makeTask(id: "task_a", name: "Apple", projectID: "proj_test")],
            chores: [makeTask(id: "chore_x", name: "Chore", projectID: nil)]
        ))
        // Chores-only hides the project task we want to reveal.
        model.filterToChoresOnly = true
        XCTAssertFalse(model.visibleWorkItems.contains { $0.id == "task_a" })

        model.revealWorkCard("task_a", productID: "prod_test")

        XCTAssertFalse(model.filterToChoresOnly, "reveal must clear the chores-only filter")
        XCTAssertTrue(model.visibleWorkItems.contains { $0.id == "task_a" })
    }

    func testRevealRestoresHiddenChoresToShowAChore() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(
            chores: [makeTask(id: "chore_x", name: "Chore", projectID: nil)]
        ))
        // Chores hidden — a chore reveal target would stay invisible.
        model.includeChores = false
        XCTAssertFalse(model.visibleWorkItems.contains { $0.id == "chore_x" })

        model.revealWorkCard("chore_x", productID: "prod_test")

        XCTAssertTrue(model.includeChores, "reveal must re-enable chores so a chore target shows")
        XCTAssertTrue(model.visibleWorkItems.contains { $0.id == "chore_x" })
    }

    func testRevealClearsProjectFilter() {
        let model = makeModel()
        model.selectedProjectFilterIDs = ["proj_other"]

        model.revealWorkCard("task_a", productID: "prod_test")

        XCTAssertTrue(model.selectedProjectFilterIDs.isEmpty, "reveal must clear the project filter")
    }

    // MARK: - Helpers

    private func makeTask(
        id: String,
        name: String,
        status: String = "todo",
        projectID: String? = "proj_test"
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: projectID,
            kind: "task",
            name: name,
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z"
        )
    }

    private func makeWorkTreeEvent(tasks: [WorkTask] = [], chores: [WorkTask] = []) -> EngineEvent {
        let project = WorkProject(
            id: "proj_test",
            productID: "prod_test",
            name: "Test Project",
            slug: "test-project",
            description: "",
            goal: "",
            status: "active",
            priority: "medium",
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z"
        )
        return .workTree(
            product: WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: "https://github.com/org/repo.git",
                status: "active",
                createdAt: "2026-05-26T00:00:00Z",
                updatedAt: "2026-05-26T00:00:00Z"
            ),
            projects: [project],
            tasks: tasks,
            chores: chores,
            taskRuntimes: [],
            dependencies: []
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
                repoRemoteURL: "https://github.com/org/repo.git",
                status: "active",
                createdAt: "2026-05-26T00:00:00Z",
                updatedAt: "2026-05-26T00:00:00Z"
            )
        ]
        model.selectWorkProduct("prod_test")
        return model
    }
}
