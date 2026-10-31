import XCTest
@testable import Boss

/// Covers revision-kind kanban routing rules:
///
/// - An `in_review` revision must NOT appear as a standalone card in the
///   Review column (it rolls up under the parent's card instead).
/// - An `in_review` revision DOES NOT appear in Backlog or Doing either
///   (it has no standalone card anywhere in that status).
/// - A revision that is `todo` or `active` appears in Backlog/Doing as a
///   distinct card — NOT hidden.
/// - `inReviewRevisions(forParentTaskID:)` returns the right revisions,
///   ordered by `revisionSeq`.
/// - `workTask(withID:)` resolves tasks from both project-task and chore
///   buckets.
@MainActor
final class RevisionKanbanTests: XCTestCase {

    // MARK: boardColumn routing

    func testInReviewRevisionRoutesMappedToReview() {
        let revision = makeRevision(status: "in_review", seq: 1)
        XCTAssertEqual(revision.boardColumn, .review,
                       "in_review revision's boardColumn should be .review")
    }

    func testTodoRevisionRoutesToBacklog() {
        let revision = makeRevision(status: "todo", seq: 1)
        XCTAssertEqual(revision.boardColumn, .backlog)
    }

    func testActiveRevisionRoutesToDoing() {
        let revision = makeRevision(status: "active", seq: 2)
        XCTAssertEqual(revision.boardColumn, .doing)
    }

    // MARK: workItems(in:) suppression

    func testInReviewRevisionSuppressedFromReviewColumn() {
        let model = makeModel()
        let parent = makeParent(status: "in_review")
        let revision = makeRevision(status: "in_review", seq: 1, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, revision]]

        let reviewItems = model.workItems(in: .review)
        XCTAssertTrue(
            reviewItems.contains(where: { $0.id == parent.id }),
            "parent must appear in Review"
        )
        XCTAssertFalse(
            reviewItems.contains(where: { $0.id == revision.id }),
            "in_review revision must NOT appear as a standalone card in Review"
        )
    }

    func testTodoRevisionNotSuppressedFromBacklog() {
        let model = makeModel()
        let revision = makeRevision(status: "todo", seq: 1)
        model.tasksByProjectID = ["proj_1": [revision]]

        let backlogItems = model.workItems(in: .backlog)
        XCTAssertTrue(
            backlogItems.contains(where: { $0.id == revision.id }),
            "todo revision must appear in Backlog"
        )
    }

    func testActiveRevisionNotSuppressedFromDoing() {
        let model = makeModel()
        let revision = makeRevision(status: "active", seq: 2)
        model.tasksByProjectID = ["proj_1": [revision]]

        let doingItems = model.workItems(in: .doing)
        XCTAssertTrue(
            doingItems.contains(where: { $0.id == revision.id }),
            "active revision must appear in Doing"
        )
    }

    // MARK: inReviewRevisions(forParentTaskID:)

    func testInReviewRevisionsReturnedForParent() {
        let model = makeModel()
        let parent = makeParent(status: "in_review")
        let r1 = makeRevision(status: "in_review", seq: 1, parentID: parent.id)
        let r2 = makeRevision(status: "in_review", seq: 2, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, r1, r2]]

        let revisions = model.inReviewRevisions(forParentTaskID: parent.id)
        XCTAssertEqual(revisions.count, 2)
        XCTAssertEqual(revisions[0].id, r1.id, "R1 should come first (ordered by seq)")
        XCTAssertEqual(revisions[1].id, r2.id, "R2 should come second")
    }

    func testInReviewRevisionsExcludesNonInReview() {
        let model = makeModel()
        let parent = makeParent(status: "in_review")
        let r1 = makeRevision(status: "in_review", seq: 1, parentID: parent.id)
        let r2 = makeRevision(status: "todo", seq: 2, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, r1, r2]]

        let revisions = model.inReviewRevisions(forParentTaskID: parent.id)
        XCTAssertEqual(revisions.count, 1)
        XCTAssertEqual(revisions[0].id, r1.id, "only in_review revisions should appear")
    }

    func testInReviewRevisionsExcludesOtherParents() {
        let model = makeModel()
        let parent1 = makeParent(status: "in_review", suffix: "1")
        let parent2 = makeParent(status: "in_review", suffix: "2")
        let r1 = makeRevision(status: "in_review", seq: 1, parentID: parent1.id)
        let r2 = makeRevision(status: "in_review", seq: 1, parentID: parent2.id)
        model.tasksByProjectID = ["proj_1": [parent1, parent2, r1, r2]]

        let revisionsForP1 = model.inReviewRevisions(forParentTaskID: parent1.id)
        XCTAssertEqual(revisionsForP1.count, 1)
        XCTAssertEqual(revisionsForP1[0].id, r1.id)

        let revisionsForP2 = model.inReviewRevisions(forParentTaskID: parent2.id)
        XCTAssertEqual(revisionsForP2.count, 1)
        XCTAssertEqual(revisionsForP2[0].id, r2.id)
    }

    func testInReviewRevisionsEmptyForNonParent() {
        let model = makeModel()
        let task = makeParent(status: "in_review")
        model.tasksByProjectID = ["proj_1": [task]]

        let revisions = model.inReviewRevisions(forParentTaskID: task.id)
        XCTAssertTrue(revisions.isEmpty, "task with no revisions should return empty list")
    }

    // MARK: project grouping

    /// Chore-parented revisions (nil projectID) must appear under the "No
    /// Project" section in project-grouped Backlog, alongside other
    /// un-projected work — kind is not a grouping axis.
    func testChoreParentedRevisionGroupsUnderNoProject() {
        let model = makeModel()
        model.workBoardGrouping = .project
        let chore = makeChore(id: "chore_c19", status: "todo")
        let revision = makeChoreParentedRevision(
            id: "task_t29", status: "todo", seq: 1, parentID: chore.id, autostart: false
        )
        model.applyEventForTest(makeWorkTreeEvent(tasks: [revision], chores: [chore]))

        let sections = model.workSections(in: .backlog)
        let titles = sections.map(\.title)
        XCTAssertTrue(titles.contains("No Project"),
                      "chore-parented revision must appear in the 'No Project' section")
        let noProjectSection = sections.first { $0.title == "No Project" }
        XCTAssertTrue(
            noProjectSection?.items.contains { $0.id == revision.id } ?? false,
            "revision must be in the No Project section"
        )
    }

    func testChoreParentedRevisionInDoingFollowsEngineMembershipInProjectMode() {
        let model = makeModel()
        model.workBoardGrouping = .project
        let chore = makeChore(id: "chore_c19", status: "active")
        let revision = makeChoreParentedRevision(
            id: "task_t29", status: "todo", seq: 1, parentID: chore.id, autostart: true
        )
        model.applyEventForTest(makeWorkTreeEvent(tasks: [revision], chores: [chore]))

        let queued = model.workSections(in: .doing)
        XCTAssertEqual(queued.map(\.title), ["Queued"])
        XCTAssertEqual(Set(queued.flatMap(\.items).map(\.id)), [chore.id, revision.id])
        XCTAssertEqual(queued.flatMap(\.items).count, 2)

        model.applyEventForTest(.workerLiveStatesList(states: [], doingSections: [revision.id: .coding]))
        let started = model.workSections(in: .doing)
        XCTAssertEqual(started.map(\.title), ["Coding", "Queued"])
        XCTAssertEqual(started[0].items.map(\.id), [revision.id])
        XCTAssertEqual(started[1].items.map(\.id), [chore.id])
    }

    // MARK: AI review of a revision stays in Doing, then folds into the parent

    /// A revision whose own run finished while its PR's automated review runs
    /// is still `active`: it stays a Doing card, in the engine's Reviewing
    /// group rather than Queued, and has no Review card.
    func testRevisionUnderAIReviewStaysInDoingReviewingGroupAndNeverInReview() {
        let model = makeModel()
        let chore = makeChore(id: "chore_c20", status: "in_review")
        var revision = makeChoreParentedRevision(id: "task_t30", status: "active", seq: 1, parentID: chore.id)
        revision.aiReviewing = true
        revision.aiReviewStage = "reviewers"
        model.applyEventForTest(makeWorkTreeEvent(tasks: [revision], chores: [chore]))
        model.applyEventForTest(.workerLiveStatesList(states: [], doingSections: [revision.id: .reviewing]))

        let doing = model.workSections(in: .doing)
        XCTAssertEqual(doing.map(\.title), ["Reviewing"], "not Queued: the review is running")
        XCTAssertEqual(doing.flatMap(\.items).map(\.id), [revision.id])
        XCTAssertFalse(model.workItems(in: .review).contains { $0.id == revision.id },
                       "a revision must never be a standalone Review card")
        XCTAssertEqual(model.workItems(in: .review).map(\.id), [chore.id])
    }

    /// The card shows the review as the engine reports it, not "Queued".
    func testRevisionUnderAIReviewShowsAIReviewInProgress() {
        var revision = makeChoreParentedRevision(id: "task_t31", status: "active", seq: 1, parentID: "chore_c21")
        revision.aiReviewing = true
        revision.aiReviewStage = "reviewers"
        XCTAssertEqual(
            AgentActivityState.forDoingCard(
                runtime: nil, liveState: nil, isDispatchPending: false,
                isResolvingConflicts: false, isRemediatingCI: false,
                isAIReviewing: revision.aiReviewing, aiReviewStage: revision.aiReviewStage
            ),
            .waiting(reason: "AI review in progress")
        )
    }

    /// The reviewers are bound to the PR, so the revision card finds them by
    /// the PR they review, and only while the engine reports the review live.
    func testRevisionUnderAIReviewLinksTheLiveReviewersOfItsPR() {
        func reviewer(_ name: String, slot: Int, pr: String) -> WorkerLiveState {
            WorkerLiveState(
                name: name, slotId: slot, runId: "review-\(slot)", model: "fixture", shellPid: 0,
                lastEventAt: nil, currentTool: nil, lastToolEndedAt: nil, activity: .working,
                liveStatus: nil, liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true,
                agentType: "review", reviewStage: "reviewers", executionKind: "pr_review",
                workItemId: "chore_c22", prURL: pr
            )
        }
        let pr = "https://github.com/org/repo/pull/250"
        let index = WorkBoardSectionItemsView.reviewerIndex(states: [
            reviewer("B", slot: 2, pr: pr),
            reviewer("A", slot: 1, pr: "https://github.com/Org/Repo/pull/250"),
            reviewer("Other", slot: 3, pr: "https://github.com/org/repo/pull/999"),
        ])
        var revision = makeChoreParentedRevision(id: "task_t32", status: "active", seq: 1, parentID: "chore_c22")
        XCTAssertTrue(WorkBoardSectionItemsView.reviewWorkers(for: revision, index: index).isEmpty,
                      "no reviewers shown until the engine reports the review as running")
        revision.aiReviewing = true
        XCTAssertEqual(WorkBoardSectionItemsView.reviewWorkers(for: revision, index: index).map(\.slotId), [1, 2])
    }

    /// Once the review is consolidated the revision reaches `in_review` and
    /// has no card in any column; the parent's card carries it inline.
    func testConsolidatedRevisionFoldsIntoParentWithNoCardInAnyColumn() {
        let model = makeModel()
        let chore = makeChore(id: "chore_c23", status: "in_review")
        let revision = makeChoreParentedRevision(id: "task_t33", status: "in_review", seq: 1, parentID: chore.id)
        model.applyEventForTest(makeWorkTreeEvent(tasks: [revision], chores: [chore]))
        model.applyEventForTest(.workerLiveStatesList(states: [], doingSections: [:]))

        XCTAssertTrue(revision.isFoldedRevision)
        for column in [WorkBoardColumnKey.backlog, .doing, .review, .done] {
            XCTAssertFalse(model.workItems(in: column).contains { $0.id == revision.id },
                           "folded revision must not be a card in \(column)")
        }
        XCTAssertEqual(model.inReviewRevisions(forParentTaskID: chore.id).map(\.id), [revision.id])
    }

    func testRevisionInDoingIsNotFolded() {
        for status in ["todo", "active", "blocked"] {
            XCTAssertFalse(makeRevision(status: status, seq: 1).isFoldedRevision, status)
        }
        XCTAssertTrue(makeRevision(status: "in_review", seq: 1).isFoldedRevision)
        XCTAssertTrue(makeRevision(status: "done", seq: 1).isFoldedRevision)
    }

    /// Project-task-parented revisions (with a projectID) must appear under
    /// their project's section, not under "No Project".
    func testProjectTaskParentedRevisionGroupsUnderProject() {
        let model = makeModel()
        model.workBoardGrouping = .project
        let revision = makeRevision(status: "todo", seq: 1)
        model.tasksByProjectID = ["proj_1": [revision]]

        let sections = model.workSections(in: .backlog)
        let titles = sections.map(\.title)
        XCTAssertFalse(titles.contains("No Project"),
                       "project-task-parented revision must NOT land in 'No Project'")
        XCTAssertTrue(titles.contains("Test Project"),
                      "revision must appear under its parent project's section")
    }

    // MARK: workTask(withID:)

    func testWorkTaskWithIDFindsProjectTask() {
        let model = makeModel()
        let task = makeParent(status: "active")
        model.tasksByProjectID = ["proj_1": [task]]
        XCTAssertEqual(model.workTask(withID: task.id)?.id, task.id)
    }

    func testWorkTaskWithIDFindsChore() {
        let model = makeModel()
        let chore = WorkTask(
            id: "chore_abc",
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "A chore",
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z"
        )
        model.choresByProductID = ["prod_test": [chore]]
        XCTAssertEqual(model.workTask(withID: chore.id)?.id, chore.id)
    }

    func testWorkTaskWithIDReturnsNilForMissing() {
        let model = makeModel()
        XCTAssertNil(model.workTask(withID: "nonexistent_id"))
    }

    // MARK: chore-parented revisions (issue #789)
    //
    // A revision inherits its project from the chain root, so a chore-parented
    // revision has no project_id. The work-tree reception used to drop every
    // project-less task, making such revisions invisible everywhere in the
    // kanban. These tests drive the real reception path (`applyEventForTest`)
    // to prove they now survive and render.

    func testChoreParentedRevisionSurvivesReceptionAsDoingCard() {
        let model = makeModel()
        let chore = makeChore(id: "chore_c19", status: "active")
        // Mirrors the repro: a todo, autostart revision (→ Doing).
        let revision = makeChoreParentedRevision(
            id: "task_t29", status: "todo", seq: 1, parentID: chore.id, autostart: true
        )
        model.applyEventForTest(makeWorkTreeEvent(tasks: [revision], chores: [chore]))

        XCTAssertEqual(
            model.productLevelRevisionsByProductID["prod_test"]?.map(\.id),
            [revision.id],
            "chore-parented revision must be retained in the product-level bucket"
        )
        XCTAssertTrue(
            model.workItems(in: .doing).contains { $0.id == revision.id },
            "chore-parented revision must render as a standalone Doing card"
        )
    }

    func testChoreParentedInReviewRevisionRollsUpUnderChore() {
        let model = makeModel()
        let chore = makeChore(id: "chore_c19", status: "in_review")
        let revision = makeChoreParentedRevision(
            id: "task_t29", status: "in_review", seq: 1, parentID: chore.id
        )
        model.applyEventForTest(makeWorkTreeEvent(tasks: [revision], chores: [chore]))

        let rollup = model.inReviewRevisions(forParentTaskID: chore.id)
        XCTAssertEqual(rollup.map(\.id), [revision.id],
                       "in-review chore-parented revision must roll up under its parent chore")

        let reviewItems = model.workItems(in: .review)
        XCTAssertTrue(reviewItems.contains { $0.id == chore.id },
                      "parent chore must appear in Review")
        XCTAssertFalse(reviewItems.contains { $0.id == revision.id },
                       "in-review revision must not also appear as a standalone Review card")
    }

    func testWorkTaskWithIDFindsChoreParentedRevision() {
        let model = makeModel()
        let chore = makeChore(id: "chore_c19", status: "in_review")
        let revision = makeChoreParentedRevision(
            id: "task_t29", status: "todo", seq: 1, parentID: chore.id
        )
        model.applyEventForTest(makeWorkTreeEvent(tasks: [revision], chores: [chore]))

        XCTAssertEqual(model.workTask(withID: revision.id)?.id, revision.id,
                       "workTask(withID:) must resolve a chore-parented revision")
    }

    // MARK: Done-lane revision rollup

    func testDoneRevisionSuppressedFromDoneColumn() {
        let model = makeModel()
        let parent = makeParent(status: "done")
        let revision = makeRevision(status: "done", seq: 1, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, revision]]

        let doneItems = model.workItems(in: .done)
        XCTAssertTrue(
            doneItems.contains(where: { $0.id == parent.id }),
            "parent must appear in Done"
        )
        XCTAssertFalse(
            doneItems.contains(where: { $0.id == revision.id }),
            "done revision must NOT appear as a standalone card in Done"
        )
    }

    func testDoneRevisionsReturnedForParent() {
        let model = makeModel()
        let parent = makeParent(status: "done")
        let r1 = makeRevision(status: "done", seq: 1, parentID: parent.id)
        let r2 = makeRevision(status: "done", seq: 2, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, r1, r2]]

        let revisions = model.doneRevisions(forParentTaskID: parent.id)
        XCTAssertEqual(revisions.count, 2)
        XCTAssertEqual(revisions[0].id, r1.id, "R1 should come first (ordered by seq)")
        XCTAssertEqual(revisions[1].id, r2.id, "R2 should come second")
    }

    func testDoneRevisionsExcludesNonDone() {
        let model = makeModel()
        let parent = makeParent(status: "done")
        let r1 = makeRevision(status: "done", seq: 1, parentID: parent.id)
        let r2 = makeRevision(status: "in_review", seq: 2, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, r1, r2]]

        let revisions = model.doneRevisions(forParentTaskID: parent.id)
        XCTAssertEqual(revisions.count, 1)
        XCTAssertEqual(revisions[0].id, r1.id, "only done revisions should appear")
    }

    func testChoreParentedDoneRevisionRollsUpUnderChore() {
        let model = makeModel()
        let chore = makeChore(id: "chore_c20", status: "done")
        let revision = makeChoreParentedRevision(
            id: "task_t30", status: "done", seq: 1, parentID: chore.id
        )
        model.applyEventForTest(makeWorkTreeEvent(tasks: [revision], chores: [chore]))

        let rollup = model.doneRevisions(forParentTaskID: chore.id)
        XCTAssertEqual(rollup.map(\.id), [revision.id],
                       "done chore-parented revision must roll up under its parent chore")

        let doneItems = model.workItems(in: .done)
        XCTAssertTrue(doneItems.contains { $0.id == chore.id },
                      "parent chore must appear in Done")
        XCTAssertFalse(doneItems.contains { $0.id == revision.id },
                       "done revision must not also appear as a standalone Done card")
    }

    // MARK: - WorkTask fields

    func testRevisionFieldsDefaultToNil() {
        let task = makeParent(status: "todo")
        XCTAssertNil(task.parentTaskId)
        XCTAssertNil(task.revisionSeq)
        XCTAssertNil(task.revisionParentPrUrl)
    }

    func testRevisionFieldsPreserved() {
        let revision = makeRevision(
            status: "in_review",
            seq: 3,
            parentID: "parent_123",
            parentPrUrl: "https://github.com/org/repo/pull/42"
        )
        XCTAssertEqual(revision.parentTaskId, "parent_123")
        XCTAssertEqual(revision.revisionSeq, 3)
        XCTAssertEqual(revision.revisionParentPrUrl, "https://github.com/org/repo/pull/42")
    }

    // MARK: - Helpers

    private func makeParent(status: String, suffix: String = "") -> WorkTask {
        WorkTask(
            id: "task_parent\(suffix)_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: "proj_1",
            kind: "chore",
            name: "Parent task\(suffix)",
            description: "Parent description",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: status == "in_review" ? "https://github.com/org/repo/pull/1" : nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z"
        )
    }

    private func makeRevision(
        status: String,
        seq: Int,
        parentID: String = "task_parent",
        parentPrUrl: String = "https://github.com/org/repo/pull/1"
    ) -> WorkTask {
        WorkTask(
            id: "task_revision_\(seq)_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: "proj_1",
            kind: "revision",
            name: "Revision \(seq) description",
            description: "Detailed revision intent",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z",
            parentTaskId: parentID,
            revisionSeq: seq,
            revisionParentPrUrl: parentPrUrl
        )
    }

    /// A chore (kind = "chore"), which has no project — the chain root in the
    /// issue #789 repro.
    private func makeChore(id: String, status: String) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Chore \(id)",
            description: "Chore description",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: status == "in_review" ? "https://github.com/org/repo/pull/250" : nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z"
        )
    }

    /// A revision whose parent is a chore: it inherits the chore's (absent)
    /// project, so `projectID` is nil — the shape the reception path used to
    /// drop.
    private func makeChoreParentedRevision(
        id: String,
        status: String,
        seq: Int,
        parentID: String,
        autostart: Bool = false
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "revision",
            name: "Revision \(seq) description",
            description: "Detailed revision intent",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z",
            autostart: autostart,
            parentTaskId: parentID,
            revisionSeq: seq,
            revisionParentPrUrl: "https://github.com/org/repo/pull/250"
        )
    }

    private func makeWorkTreeEvent(
        tasks: [WorkTask] = [],
        chores: [WorkTask] = []
    ) -> EngineEvent {
        .workTree(
            product: WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-26T00:00:00Z",
                updatedAt: "2026-05-26T00:00:00Z"
            ),
            projects: [],
            tasks: tasks,
            chores: chores,
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
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-26T00:00:00Z",
                updatedAt: "2026-05-26T00:00:00Z"
            )
        ]
        model.projectsByProductID = [
            "prod_test": [
                WorkProject(
                    id: "proj_1",
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
            ]
        ]
        model.selectedWorkProductID = "prod_test"
        // Chores (and chore-parented revisions) are gated on this; pin it on so
        // tests don't depend on the machine's persisted UserDefaults value.
        model.includeChores = true
        return model
    }
}
