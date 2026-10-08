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

    /// `applyIncrementalTaskUpdate` evicts the row from every bucket and
    /// re-inserts the wire payload verbatim. A complete payload that still
    /// carries `hasInProgressRevision` must survive bucket eviction.
    func testWorkItemUpdatedCompletePayloadSurvivesBucketEviction() {
        let model = makeModel()
        var seeded = makeTask(id: "task_x", projectID: "proj_a")
        seeded.status = "in_review"
        seeded.prURL = "https://github.com/org/repo/pull/9"
        seeded.hasInProgressRevision = true
        seeded.hasAttachments = true
        model.applyEventForTest(makeWorkTreeEvent(tasks: [seeded]))
        XCTAssertEqual(
            model.tasksByProjectID["proj_a"]?.first?.hasInProgressRevision,
            true
        )

        var updated = seeded
        updated.hasInProgressRevision = true
        updated.hasAttachments = true
        model.applyEventForTest(.workItemUpdated(item: .task(updated)))

        let after = model.tasksByProjectID["proj_a"]?.first { $0.id == seeded.id }
        XCTAssertEqual(after?.hasInProgressRevision, true)
        XCTAssertEqual(after?.hasAttachments, true)
    }

    func testProjectFollowupStaysInProjectAfterChoreUpdate() {
        let model = makeModel()
        let followup = makeTask(id: "followup", projectID: "proj_a", kind: "followup")
        model.applyEventForTest(makeWorkTreeEvent(tasks: [followup]))
        XCTAssertEqual(model.task(withID: followup.id)?.status, "todo")

        var updated = followup
        updated.status = "active"
        model.applyEventForTest(.workItemUpdated(item: .chore(updated)))

        XCTAssertEqual(model.tasksByProjectID["proj_a"]?.map(\.id), [followup.id])
        XCTAssertEqual(model.task(withID: followup.id)?.status, "active")
        XCTAssertTrue(model.choresByProductID.values.flatMap { $0 }.isEmpty)
        XCTAssertTrue(model.productLevelTasksByProductID.values.flatMap { $0 }.isEmpty)
        XCTAssertTrue(model.productLevelRevisionsByProductID.values.flatMap { $0 }.isEmpty)
        XCTAssertFalse(ChatViewModel.incrementalUpdateRequiresFullInvalidation(
            previous: followup, updated: updated, isChore: true
        ))

        model.applyEventForTest(makeWorkTreeEvent(tasks: [updated]))
        XCTAssertEqual(model.tasksByProjectID["proj_a"]?.map(\.id), [followup.id])
    }

    func testMoveTaskInProjectIncludesFollowupsInReorder() {
        let model = makeModel()
        var first = makeTask(id: "first", projectID: "proj_a", kind: "project_task")
        first.ordinal = 1
        var second = makeTask(id: "second", projectID: "proj_a", kind: "project_task")
        second.ordinal = 3
        var followup = makeTask(id: "followup", projectID: "proj_a", kind: "followup")
        followup.ordinal = 2
        var design = makeTask(id: "design", projectID: "proj_a", kind: "design")
        design.ordinal = 0
        model.applyEventForTest(makeWorkTreeEvent(tasks: [design, first, followup, second]))
        model.selectedWorkCardID = first.id
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }

        model.moveSelectedTask(offset: 1)

        XCTAssertEqual(sent.count, 1)
        XCTAssertEqual(sent.first?["type"] as? String, "reorder_project_tasks")
        XCTAssertEqual(sent.first?["project_id"] as? String, "proj_a")
        XCTAssertEqual(sent.first?["task_ids"] as? [String], [followup.id, first.id, second.id])
    }

    // MARK: - Helpers

    private func makeTask(id: String, projectID: String?, kind: String = "task") -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: projectID,
            kind: kind,
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
            dependencies: [],
            ideas: []
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
