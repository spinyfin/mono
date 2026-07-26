import XCTest
@testable import Boss

/// Correctness coverage for keyed work-cache invalidation (design entry 8).
/// A same-bucket single-item update must patch `taskIndexByID` and leave
/// dependency / revision caches intact when edges and revision rows do not
/// change. Project membership, kind, and dependency-edge changes must still
/// fully invalidate every derived cache.
@MainActor
final class WorkCacheInvalidationTests: XCTestCase {

    // MARK: - Correctness: full invalidation required

    func testProjectMembershipChangeFullyInvalidatesCaches() {
        let model = makeModel()
        let task = makeTask(id: "task_x", projectID: "proj_a")
        seedWorkTree(model, tasks: [task])
        warmAllCaches(model, parentIDForRevisions: "task_x")

        XCTAssertNotNil(model.taskIndexByID)
        XCTAssertNotNil(model.cachedDependencyPrereqs)
        XCTAssertNotNil(model.cachedInReviewRevisionsByParentID)

        let moved = makeTask(id: "task_x", projectID: "proj_b", status: task.status)
        model.applyEventForTest(.workItemUpdated(item: .task(moved)))

        assertFullyInvalidated(model, context: "project membership change")
        // Store still reflects the move; lazy rebuild can rehydrate later.
        XCTAssertEqual(model.task(withID: "task_x")?.projectID, "proj_b")
    }

    func testKindChangeFullyInvalidatesCaches() {
        let model = makeModel()
        // Product-level investigation in the product-level-task bucket.
        let investigation = makeTask(
            id: "inv_1",
            projectID: nil,
            kind: "investigation",
            status: "todo"
        )
        seedWorkTree(model, tasks: [investigation])
        warmAllCaches(model, parentIDForRevisions: "inv_1")

        XCTAssertNotNil(model.taskIndexByID)
        XCTAssertNotNil(model.cachedDependencyPrereqs)

        // Kind flip routes the row into the product-level revision bucket.
        var asRevision = makeTask(
            id: "inv_1",
            projectID: nil,
            kind: "revision",
            status: "todo"
        )
        asRevision.parentTaskId = "parent_missing"
        model.applyEventForTest(.workItemUpdated(item: .task(asRevision)))

        assertFullyInvalidated(model, context: "kind change")
        XCTAssertEqual(model.task(withID: "inv_1")?.kind, "revision")
    }

    func testDependencyEdgeChangeFullyInvalidatesCaches() {
        let model = makeModel()
        let taskA = makeTask(id: "task_a", projectID: "proj_a")
        let taskB = makeTask(id: "task_b", projectID: "proj_a")
        seedWorkTree(model, tasks: [taskA, taskB], dependencies: [])
        warmAllCaches(model, parentIDForRevisions: "task_a")

        XCTAssertNotNil(model.taskIndexByID)
        XCTAssertNotNil(model.cachedDependencyPrereqs)
        XCTAssertNotNil(model.cachedGatingPrereqs)

        // Edge list change goes through `dependenciesByProductID.didSet`,
        // which must still full-invalidate (not a keyed single-item path).
        model.dependenciesByProductID = [
            "prod_test": [
                WorkItemDependency(
                    dependentID: "task_b",
                    prerequisiteID: "task_a",
                    relation: "blocks"
                ),
            ],
        ]

        assertFullyInvalidated(model, context: "dependency edge change")
        // Rebuild surfaces the new edge.
        XCTAssertEqual(
            model.dependencyPrereqs(for: "task_b").map(\.id),
            ["task_a"]
        )
    }

    // MARK: - Keyed path: same-bucket field update

    func testSameBucketStatusUpdatePatchesTaskIndexAndPreservesRevisionCache() {
        let model = makeModel()
        let task = makeTask(id: "task_x", projectID: "proj_a", status: "todo")
        // Unrelated revision so the revision cache has a real entry that
        // a non-revision status flip must not discard.
        var revision = makeTask(
            id: "rev_1",
            projectID: "proj_a",
            kind: "revision",
            status: "in_review"
        )
        revision.parentTaskId = "task_x"
        revision.revisionSeq = 1
        seedWorkTree(model, tasks: [task, revision])

        _ = model.task(withID: "task_x")
        _ = model.inReviewRevisions(forParentTaskID: "task_x")
        _ = model.dependencyPrereqsByTaskID
        XCTAssertNotNil(model.taskIndexByID)
        XCTAssertNotNil(model.cachedInReviewRevisionsByParentID)
        let revisionCacheBefore = model.cachedInReviewRevisionsByParentID

        let activated = makeTask(id: "task_x", projectID: "proj_a", status: "active")
        model.applyEventForTest(.workItemUpdated(item: .task(activated)))

        // Index stays live and reflects the new status (patched, not rebuilt
        // from nil).
        XCTAssertNotNil(
            model.taskIndexByID,
            "same-bucket update must patch taskIndexByID in place, not drop it"
        )
        XCTAssertEqual(model.taskIndexByID?["task_x"]?.status, "active")

        // Non-revision status flip does not touch revision rows.
        XCTAssertNotNil(
            model.cachedInReviewRevisionsByParentID,
            "revision cache must survive a non-revision same-bucket update"
        )
        XCTAssertEqual(
            model.cachedInReviewRevisionsByParentID?["task_x"]?.map(\.id),
            revisionCacheBefore?["task_x"]?.map(\.id)
        )

        // Board layout is always dropped so columns recompute.
        XCTAssertTrue(
            model.cachedItemsByColumn.isEmpty,
            "board layout caches must still invalidate on status change"
        )
    }

    func testNameOnlyUpdateDoesNotDropDependencyCaches() {
        let model = makeModel()
        let taskA = makeTask(id: "task_a", projectID: "proj_a", status: "todo")
        let taskB = makeTask(id: "task_b", projectID: "proj_a", status: "blocked")
        let edge = WorkItemDependency(
            dependentID: "task_b",
            prerequisiteID: "task_a",
            relation: "blocks"
        )
        seedWorkTree(model, tasks: [taskA, taskB], dependencies: [edge])

        _ = model.task(withID: "task_a")
        _ = model.dependencyPrereqsByTaskID
        _ = model.gatingPrereqsByTaskID
        XCTAssertNotNil(model.cachedDependencyPrereqs)
        XCTAssertNotNil(model.cachedGatingPrereqs)

        var renamed = makeTask(id: "task_a", projectID: "proj_a", status: "todo")
        renamed.name = "Renamed A"
        model.applyEventForTest(.workItemUpdated(item: .task(renamed)))

        XCTAssertNotNil(
            model.taskIndexByID,
            "name-only update must keep the patched id index"
        )
        XCTAssertEqual(model.taskIndexByID?["task_a"]?.name, "Renamed A")
        XCTAssertNotNil(
            model.cachedDependencyPrereqs,
            "name-only update must not drop dependency caches (edges unchanged)"
        )
        XCTAssertNotNil(
            model.cachedGatingPrereqs,
            "name-only update must not drop gating caches (edges unchanged)"
        )
    }

    func testRevisionStatusChangeDropsRevisionCachesOnly() {
        let model = makeModel()
        let parent = makeTask(id: "task_x", projectID: "proj_a", status: "in_review")
        var revision = makeTask(
            id: "rev_1",
            projectID: "proj_a",
            kind: "revision",
            status: "in_review"
        )
        revision.parentTaskId = "task_x"
        revision.revisionSeq = 1
        seedWorkTree(model, tasks: [parent, revision])

        _ = model.task(withID: "rev_1")
        _ = model.inReviewRevisions(forParentTaskID: "task_x")
        _ = model.dependencyPrereqsByTaskID
        XCTAssertNotNil(model.cachedInReviewRevisionsByParentID)
        XCTAssertNotNil(model.cachedDependencyPrereqs)

        var done = revision
        done.status = "done"
        model.applyEventForTest(.workItemUpdated(item: .task(done)))

        XCTAssertNotNil(
            model.taskIndexByID,
            "same-bucket revision status flip still patches the id index"
        )
        XCTAssertNil(
            model.cachedInReviewRevisionsByParentID,
            "revision row change must drop the revision rollup caches"
        )
        XCTAssertNil(
            model.cachedDoneRevisionsByParentID,
            "revision row change must drop both in-review and done caches"
        )
        // Status change also rebuilds dep graphs (satisfaction), but the
        // index was patched rather than nil'd — membership unchanged.
        XCTAssertEqual(model.task(withID: "rev_1")?.status, "done")
        XCTAssertEqual(
            model.doneRevisions(forParentTaskID: "task_x").map(\.id),
            ["rev_1"]
        )
    }

    // MARK: - Predicate helpers

    func testIncrementalUpdateRequiresFullInvalidationPredicate() {
        let base = makeTask(id: "t", projectID: "proj_a", kind: "task")
        XCTAssertTrue(
            ChatViewModel.incrementalUpdateRequiresFullInvalidation(
                previous: nil, updated: base, isChore: false
            ),
            "unknown previous must full-invalidate"
        )
        XCTAssertFalse(
            ChatViewModel.incrementalUpdateRequiresFullInvalidation(
                previous: base, updated: base, isChore: false
            )
        )
        let moved = makeTask(id: "t", projectID: "proj_b", kind: "task")
        XCTAssertTrue(
            ChatViewModel.incrementalUpdateRequiresFullInvalidation(
                previous: base, updated: moved, isChore: false
            )
        )
        let kindFlip = makeTask(id: "t", projectID: nil, kind: "revision")
        XCTAssertTrue(
            ChatViewModel.incrementalUpdateRequiresFullInvalidation(
                previous: base, updated: kindFlip, isChore: false
            )
        )
        let chore = makeTask(id: "t", projectID: nil, kind: "chore")
        XCTAssertTrue(
            ChatViewModel.incrementalUpdateRequiresFullInvalidation(
                previous: base, updated: chore, isChore: true
            )
        )
    }

    func testTaskUpdateAffectsRevisionCachePredicate() {
        let model = makeModel()
        let task = makeTask(id: "t", projectID: "proj_a", kind: "task", status: "todo")
        XCTAssertFalse(model.taskUpdateAffectsRevisionCache(previous: task, updated: task))

        var active = task
        active.status = "active"
        XCTAssertFalse(
            model.taskUpdateAffectsRevisionCache(previous: task, updated: active),
            "non-revision status flip must not touch revision caches"
        )

        var revision = makeTask(id: "r", projectID: "proj_a", kind: "revision", status: "in_review")
        revision.parentTaskId = "t"
        var done = revision
        done.status = "done"
        XCTAssertTrue(model.taskUpdateAffectsRevisionCache(previous: revision, updated: done))
    }

    // MARK: - Helpers

    private func assertFullyInvalidated(_ model: ChatViewModel, context: String) {
        XCTAssertNil(model.taskIndexByID, "\(context): taskIndexByID must be nil")
        XCTAssertNil(model.cachedDependencyPrereqs, "\(context): dependency cache must be nil")
        XCTAssertNil(model.cachedGatingPrereqs, "\(context): gating cache must be nil")
        XCTAssertNil(
            model.cachedInReviewRevisionsByParentID,
            "\(context): in-review revision cache must be nil"
        )
        XCTAssertNil(
            model.cachedDoneRevisionsByParentID,
            "\(context): done revision cache must be nil"
        )
        XCTAssertTrue(
            model.cachedItemsByColumn.isEmpty,
            "\(context): per-column item cache must be empty"
        )
        XCTAssertTrue(
            model.cachedSectionsByColumn.isEmpty,
            "\(context): per-column section cache must be empty"
        )
        XCTAssertNil(
            model.cachedWorkBoardRepoMode,
            "\(context): workBoardRepoMode cache must be nil"
        )
    }

    private func warmAllCaches(_ model: ChatViewModel, parentIDForRevisions: String) {
        _ = model.task(withID: parentIDForRevisions)
        _ = model.dependencyPrereqsByTaskID
        _ = model.gatingPrereqsByTaskID
        _ = model.inReviewRevisions(forParentTaskID: parentIDForRevisions)
        _ = model.doneRevisions(forParentTaskID: parentIDForRevisions)
        _ = model.workItems(in: .backlog)
        _ = model.workSections(in: .backlog)
        _ = model.workBoardRepoMode
    }

    private func seedWorkTree(
        _ model: ChatViewModel,
        tasks: [WorkTask],
        dependencies: [WorkItemDependency] = []
    ) {
        model.applyEventForTest(makeWorkTreeEvent(tasks: tasks, dependencies: dependencies))
    }

    private func makeTask(
        id: String,
        projectID: String?,
        kind: String = "task",
        status: String = "todo"
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: projectID,
            kind: kind,
            name: "Task \(id)",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-07-10T00:00:00Z",
            updatedAt: "2026-07-10T00:00:00Z"
        )
    }

    private func makeWorkTreeEvent(
        tasks: [WorkTask],
        dependencies: [WorkItemDependency]
    ) -> EngineEvent {
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
            dependencies: dependencies
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
            ),
        ]
        model.selectWorkProduct("prod_test")
        return model
    }
}
