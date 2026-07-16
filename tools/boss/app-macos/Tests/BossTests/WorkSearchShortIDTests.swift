import XCTest
@testable import Boss

/// Coverage for the main window search matching a work item's short id
/// (e.g. typing "2801" or "T2801" finds T2801), not just its name/description.
@MainActor
final class WorkSearchShortIDTests: XCTestCase {

    func testBareDigitsMatchShortID() {
        let model = makeModel()
        model.applyEventForTest(
            makeWorkTreeEvent(tasks: [
                makeTask(id: "task_a", name: "Alpha", shortID: 2801),
                makeTask(id: "task_b", name: "Beta", shortID: 42),
            ])
        )

        model.workSearchText = "2801"
        XCTAssertEqual(model.visibleWorkItems.map(\.id), ["task_a"])
    }

    func testPrefixedFormMatchesCaseInsensitively() {
        let model = makeModel()
        model.applyEventForTest(
            makeWorkTreeEvent(tasks: [
                makeTask(id: "task_a", name: "Alpha", shortID: 2801),
                makeTask(id: "task_b", name: "Beta", shortID: 42),
            ])
        )

        model.workSearchText = "T2801"
        XCTAssertEqual(model.visibleWorkItems.map(\.id), ["task_a"])

        model.workSearchText = "t2801"
        XCTAssertEqual(model.visibleWorkItems.map(\.id), ["task_a"])
    }

    func testExactShortIDMatchIsRankedFirst() {
        let model = makeModel()
        model.applyEventForTest(
            makeWorkTreeEvent(tasks: [
                // "T280" contains "28" as a substring, so a prefix search for
                // "28" also surfaces it — but the exact id (T28) must lead.
                makeTask(id: "task_prefix", name: "Prefix match", shortID: 280),
                makeTask(id: "task_exact", name: "Exact match", shortID: 28),
            ])
        )

        model.workSearchText = "28"
        XCTAssertEqual(model.visibleWorkItems.map(\.id), ["task_exact", "task_prefix"])
    }

    func testPlainTextSearchIsUnaffected() {
        let model = makeModel()
        model.applyEventForTest(
            makeWorkTreeEvent(tasks: [
                makeTask(id: "task_a", name: "Alpha", shortID: 2801),
                makeTask(id: "task_b", name: "Beta", shortID: 42),
            ])
        )

        model.workSearchText = "Alpha"
        XCTAssertEqual(model.visibleWorkItems.map(\.id), ["task_a"])
    }

    func testParseShortIDQuery() {
        XCTAssertEqual(ChatViewModel.parseShortIDQuery("2801"), 2801)
        XCTAssertEqual(ChatViewModel.parseShortIDQuery("T2801"), 2801)
        XCTAssertEqual(ChatViewModel.parseShortIDQuery("t2801"), 2801)
        XCTAssertNil(ChatViewModel.parseShortIDQuery("Alpha"))
        XCTAssertNil(ChatViewModel.parseShortIDQuery("T"))
        XCTAssertNil(ChatViewModel.parseShortIDQuery(""))
        XCTAssertNil(ChatViewModel.parseShortIDQuery("T28a1"))
    }

    // MARK: - Helpers

    private func makeTask(id: String, name: String, shortID: Int) -> WorkTask {
        var task = WorkTask(
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
        task.shortID = shortID
        return task
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
