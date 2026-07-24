import XCTest
@testable import Boss

/// Regression coverage for the `trackedProjectIDsByProductID` /
/// `scheduleWorkTreeRefetch` changes that replaced two full-dictionary
/// O(all-products) scans with targeted, tracked-set eviction and a
/// per-product debounce (see `ChatViewModel+WorkItemEvents.swift` and
/// `ChatViewModel+EventHandling.swift`).
///
/// `applyWorkTree` only evicts the project buckets it has previously
/// recorded in `trackedProjectIDsByProductID`. `applyIncrementalTaskUpdate`
/// (the `workItemUpdated` path) can also populate a `tasksByProjectID`
/// bucket out-of-band — when a task moves into a project that was
/// previously empty (and therefore untracked), that project must be added
/// to the tracked set immediately, or the next full `applyWorkTree` will
/// fail to evict the bucket before appending, duplicating the card.
@MainActor
final class WorkTreeApplyEvictionTests: XCTestCase {

    func testTaskMovedIntoPreviouslyEmptyProjectIsNotDuplicatedOnNextFullApply() {
        let model = makeModel()

        // (1) Full apply: project A has the task, project B is empty (and
        // therefore untracked — only projects with tasks land in
        // trackedProjectIDsByProductID).
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeTask(id: "task_x", projectID: "proj_a"),
        ]))
        XCTAssertEqual(model.tasksByProjectID["proj_a"]?.map(\.id), ["task_x"])
        XCTAssertNil(model.tasksByProjectID["proj_b"])

        // (2) Incremental update moves the task into the previously-empty
        // project B.
        let moved = makeTask(id: "task_x", projectID: "proj_b")
        model.applyEventForTest(.workItemUpdated(item: .task(moved)))
        XCTAssertEqual(model.tasksByProjectID["proj_b"]?.map(\.id), ["task_x"])

        // (3) A subsequent full apply (e.g. workInvalidated / planner
        // action / re-selection) resends the same state. Project B must be
        // evicted before the task is re-appended, or it duplicates.
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeTask(id: "task_x", projectID: "proj_b"),
        ]))

        XCTAssertEqual(
            model.tasksByProjectID["proj_b"]?.map(\.id), ["task_x"],
            "moving a task into a previously-empty project must not leave a duplicate card behind a later full apply"
        )
        XCTAssertNil(
            model.tasksByProjectID["proj_a"],
            "the vacated project's stale bucket must be evicted by the full apply"
        )
    }

    func testBurstOfInvalidationTriggersCollapsesToOneScheduledRefetch() async {
        let model = makeModel()

        model.scheduleWorkTreeRefetch(productID: "prod_test", flow: .invalidationRefetch)
        let firstTask = model.pendingWorkTreeRefetchTasks["prod_test"]
        XCTAssertNotNil(firstTask)

        // A burst of further triggers within the debounce window (mirrors
        // repeated workItemDeleted / projectTasksReordered pushes) must
        // cancel-and-replace rather than queue additional fetches.
        model.scheduleWorkTreeRefetch(productID: "prod_test", flow: .invalidationRefetch)
        model.scheduleWorkTreeRefetch(productID: "prod_test", flow: .invalidationRefetch)
        let lastTask = model.pendingWorkTreeRefetchTasks["prod_test"]

        XCTAssertEqual(firstTask?.isCancelled, true, "an earlier trigger's task must be cancelled by a later one")
        XCTAssertEqual(lastTask?.isCancelled, false)

        // After the debounce window elapses, exactly one pending refetch
        // fires and clears itself.
        try? await Task.sleep(nanoseconds: 250_000_000)
        XCTAssertNil(
            model.pendingWorkTreeRefetchTasks["prod_test"],
            "the coalesced refetch must fire once and remove itself from the pending map"
        )
    }

    // MARK: - Helpers

    private func makeTask(id: String, projectID: String?) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: projectID,
            kind: "task",
            name: "Task \(id)",
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-07-10T00:00:00Z",
            updatedAt: "2026-07-10T00:00:00Z"
        )
    }

    private func makeWorkTreeEvent(tasks: [WorkTask]) -> EngineEvent {
        let projectA = WorkProject(
            id: "proj_a",
            productID: "prod_test",
            name: "Project A",
            slug: "project-a",
            description: "",
            goal: "",
            status: "active",
            priority: "medium",
            createdAt: "2026-07-10T00:00:00Z",
            updatedAt: "2026-07-10T00:00:00Z"
        )
        let projectB = WorkProject(
            id: "proj_b",
            productID: "prod_test",
            name: "Project B",
            slug: "project-b",
            description: "",
            goal: "",
            status: "active",
            priority: "medium",
            createdAt: "2026-07-10T00:00:00Z",
            updatedAt: "2026-07-10T00:00:00Z"
        )
        return .workTree(
            product: WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: "https://github.com/org/repo.git",
                status: "active",
                createdAt: "2026-07-10T00:00:00Z",
                updatedAt: "2026-07-10T00:00:00Z"
            ),
            projects: [projectA, projectB],
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
                createdAt: "2026-07-10T00:00:00Z",
                updatedAt: "2026-07-10T00:00:00Z"
            )
        ]
        model.selectWorkProduct("prod_test")
        return model
    }
}
