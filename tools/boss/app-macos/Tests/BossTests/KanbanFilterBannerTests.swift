import XCTest
@testable import Boss

/// Coverage for issue #1248: when a search filter is active the board hides
/// non-matching cards, and without a standing indicator a stale query reads
/// as an empty/complete board — a card looks deleted when it is merely
/// filtered out. `ChatViewModel.activeWorkSearchQuery` / `isWorkSearchActive`
/// back the persistent "Filtered view" banner; these tests assert that
/// signal tracks the search text exactly and stays consistent with the
/// actual filtering of `visibleWorkItems`.
@MainActor
final class KanbanFilterBannerTests: XCTestCase {

    func testNoBannerSignalWhenSearchEmpty() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [makeTask(id: "task_a", name: "Alpha")]))

        XCTAssertFalse(model.isWorkSearchActive, "an empty search must not flag the board as filtered")
        XCTAssertNil(model.activeWorkSearchQuery)
    }

    func testWhitespaceOnlySearchIsNotActive() {
        let model = makeModel()
        model.workSearchText = "   \n\t "

        XCTAssertFalse(
            model.isWorkSearchActive,
            "a whitespace-only query hides nothing, so the board is not filtered"
        )
        XCTAssertNil(model.activeWorkSearchQuery)
    }

    func testActiveSearchExposesTrimmedQuery() {
        let model = makeModel()
        model.workSearchText = "  Alpha  "

        XCTAssertTrue(model.isWorkSearchActive, "a non-empty query must flag the board as filtered")
        XCTAssertEqual(
            model.activeWorkSearchQuery,
            "Alpha",
            "the banner query must be trimmed of surrounding whitespace"
        )
    }

    func testSearchSignalMatchesActualFiltering() {
        let model = makeModel()
        model.applyEventForTest(
            makeWorkTreeEvent(tasks: [
                makeTask(id: "task_a", name: "Alpha"),
                makeTask(id: "task_b", name: "Beta"),
            ])
        )

        // Filtering down to one card must coincide with the banner signal:
        // the hidden card (Beta) must never silently vanish without the
        // board being flagged as filtered.
        model.workSearchText = "Alpha"
        XCTAssertTrue(model.isWorkSearchActive)
        XCTAssertEqual(model.visibleWorkItems.map(\.id), ["task_a"])
        XCTAssertFalse(
            model.visibleWorkItems.contains { $0.id == "task_b" },
            "Beta is filtered out — the banner signal is what tells the user why"
        )

        // Clearing the search restores every card and lowers the banner.
        model.workSearchText = ""
        XCTAssertFalse(model.isWorkSearchActive)
        XCTAssertEqual(Set(model.visibleWorkItems.map(\.id)), ["task_a", "task_b"])
    }

    // MARK: - Helpers

    private func makeTask(id: String, name: String) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: "proj_test",
            kind: "task",
            name: name,
            description: "",
            status: "backlog",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z"
        )
    }

    private func makeWorkTreeEvent(tasks: [WorkTask] = []) -> EngineEvent {
        .workTree(
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
            projects: [
                WorkProject(
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
            ],
            tasks: tasks,
            chores: [],
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
