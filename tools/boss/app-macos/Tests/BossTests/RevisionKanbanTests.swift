import XCTest
import SwiftUI
@testable import Boss

/// Revisions in review have standalone cards with the primary presentation.
@MainActor
final class RevisionKanbanTests: XCTestCase {

    func testReviewRevisionSharesReviewWorkersAndPRActions() {
        let model = makeModel()
        let primary = makeParent(status: "in_review")
        var revision = makeRevision(status: "in_review", seq: 1, parentID: primary.id)
        revision.prURL = primary.prURL
        model.tasksByProjectID = ["proj_1": [primary, revision]]
        XCTAssertEqual(model.reviewOwner(for: revision).id, primary.id)
        func worker(_ id: String, slot: Int, pr: String?, stage: String = "reviewers") -> WorkerLiveState {
            WorkerLiveState(
                slotId: slot, runId: id, model: "fixture", shellPid: 0, lastEventAt: nil,
                currentTool: nil, lastToolEndedAt: nil, activity: .working, liveStatus: nil,
                liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true,
                agentType: "review", reviewStage: stage, executionKind: "pr_review", workItemId: primary.id, prURL: pr
            )
        }
        let states = [worker("second", slot: 26, pr: primary.prURL),
                      worker("first", slot: 25, pr: "https://www.github.com/ORG/REPO/pull/1/"),
                      worker("other", slot: 27, pr: "https://github.com/other/repo/pull/1")]
        for task in [primary, revision] {
            XCTAssertEqual(WorkBoardSectionItemsView.reviewWorkers(for: task, index: WorkBoardSectionItemsView.reviewerIndex(states: states)).map(\.runId), ["first", "second"])
            let supervisor = worker("supervisor", slot: 28, pr: primary.prURL, stage: "consolidation")
            XCTAssertEqual(WorkBoardSectionItemsView.reviewWorkers(for: task, index: WorkBoardSectionItemsView.reviewerIndex(states: [supervisor])).map(\.runId), ["supervisor"])
            XCTAssertTrue(WorkBoardSectionItemsView.reviewWorkers(for: task, index: [:]).isEmpty)
            let link = ReviewWorkerLink(reviewer: supervisor, model: model)
            XCTAssertEqual(link.title, "Worker supervisor — Consolidating")
            XCTAssertEqual(link.tooltip, "Show live review worker")
            let host = NSHostingView(rootView: link)
            XCTAssertGreaterThan(host.fittingSize.width, 0)
            var selectedRun: String?
            model.paneFocusHandler = { selectedRun = $0; return .success }
            model.navigationMode = .work
            link.activate()
            XCTAssertEqual(selectedRun, supervisor.runId)
            XCTAssertEqual(model.navigationMode, .agents)

        }
    }

    func testSerializedReviewCardsSurviveTreeAndSingleItemReception() async throws {
        for section in ReviewSection.allCases {
            let model = makeModel()
            let client = EngineClient(socketPath: "/tmp/review-wire-\(UUID().uuidString).sock")
            let blocked = section == .blocked
            let status = blocked ? "blocked" : "in_review"
            let reason = blocked ? "\"merge_conflict\"" : "null"
            let detail = blocked ? "\"conflicts in a.rs\"" : "null"
            let aiState = section == .aiReview ? "reviewing" : "reviewed_all_clear"
            let reviewing = section == .aiReview
            let badgeLabel = reviewing ? "Reviewing (AI)" : "Reviewed"
            let ciState = section == .pending ? "in_progress" : "success"
            let ready = section == .ready
            let inRevision = section == .inRevision
            // Independent engine-shaped records, including the held revision's
            // projected status rather than its durable active status.
            let primary = """
            {"id":"task_primary","product_id":"prod_test","project_id":null,"kind":"chore","name":"Primary","description":"","status":"\(status)","priority":"medium","created_at":"2026-10-01","updated_at":"2026-10-01","pr_url":"https://github.com/org/repo/pull/1","review_section":"\(section.rawValue)","blocked_reason":\(reason),"blocked_detail":\(detail),"ci_required_state":"\(ciState)","review_required_state":"pending","ai_review_state":"\(aiState)","ai_reviewing":\(reviewing),"ai_review_badge":{"label":"\(badgeLabel)","system_image":"sparkles","tooltip":"Shared PR review"},"ready_for_review":\(ready),"has_in_progress_revision":\(inRevision)}
            """
            let revision = """
            {"id":"task_revision","product_id":"prod_test","project_id":null,"kind":"revision","name":"Revision","description":"Review fixes","status":"\(status)","priority":"medium","created_at":"2026-10-02","updated_at":"2026-10-02","parent_task_id":"task_primary","revision_seq":1,"pr_url":"https://github.com/org/repo/pull/1","review_section":"\(section.rawValue)","blocked_reason":\(reason),"blocked_detail":\(detail),"ci_required_state":"\(ciState)","review_required_state":"pending","ai_review_state":"\(aiState)","ai_reviewing":\(reviewing),"ai_review_badge":{"label":"\(badgeLabel)","system_image":"sparkles","tooltip":"Shared PR review"},"ready_for_review":\(ready),"has_in_progress_revision":\(inRevision)}
            """
            func receive(_ payload: String) async {
                let received = expectation(description: "decoded and reduced")
                client.onEvent = { event in
                    model.applyEventForTest(event)
                    received.fulfill()
                }
                client.consumeLineForTesting("{\"payload\":\(payload)}")
                await fulfillment(of: [received], timeout: 3)
            }
            await receive("""
            {"type":"work_tree","product":{"id":"prod_test","name":"Test","slug":"test","description":"","status":"active","created_at":"2026-10-01","updated_at":"2026-10-01"},"projects":[],"tasks":[\(revision)],"chores":[\(primary)],"task_runtimes":[],"dependencies":[],"ideas":[]}
            """)
            for updated in [false, true] {
                if updated {
                    await receive("{\"type\":\"work_item_updated\",\"item\":{\"item_type\":\"task\",\(revision.dropFirst())}")
                }
                let items = model.workItems(in: .review)
                XCTAssertEqual(Set(items.map(\.id)), ["task_primary", "task_revision"])
                let groups = ChatViewModel.reviewSections(items: items)
                XCTAssertEqual(groups.count, 1)
                XCTAssertEqual(groups.first?.title, section.title)
                XCTAssertEqual(Set(groups[0].items.map(\.id)), Set(items.map(\.id)))
                let parent = try XCTUnwrap(model.workTask(withID: "task_primary"))
                let child = try XCTUnwrap(model.workTask(withID: "task_revision"))
                XCTAssertEqual(child.boardColumn, parent.boardColumn)
                XCTAssertEqual(child.reviewSection, section)
                func snapshot(_ task: WorkTask) -> WorkCardSnapshot {
                    model.workCardSnapshot(for: task, column: .review, isSelected: false,
                        isFrontierHighlighted: false, boardStyle: .classic, liveState: nil)
                }
                let root = snapshot(parent)
                let card = snapshot(child)
                XCTAssertEqual(card.blockedBadgeText, root.blockedBadgeText)
                XCTAssertEqual(card.blockedBadgeTooltip, root.blockedBadgeTooltip)
                XCTAssertEqual(card.showsBlockedChrome, root.showsBlockedChrome)
                XCTAssertEqual(card.showsMergeWhenReady, root.showsMergeWhenReady)
                XCTAssertEqual(card.ciRequiredState, root.ciRequiredState)
                XCTAssertEqual(card.ciRequiredState, ciState)
                XCTAssertEqual(child.aiReviewing, reviewing)
                XCTAssertEqual(card.isAIReviewing, root.isAIReviewing)
                XCTAssertFalse(card.isAIReviewing, "the legacy live-status flag belongs to Doing")
                XCTAssertEqual(card.aiReviewState, aiState)
                XCTAssertEqual(card.aiReviewBadge?.label, badgeLabel)
                XCTAssertEqual(card.aiReviewBadge, root.aiReviewBadge)
                XCTAssertEqual(card.aiReviewState, root.aiReviewState)
                XCTAssertTrue(root.doneRevisions.isEmpty)
                if blocked {
                    XCTAssertEqual(card.blockedBadgeText, "Merge Conflict")
                    XCTAssertNotNil(card.blockedBadgeTooltip)
                    XCTAssertTrue(card.showsBlockedChrome)
                    XCTAssertFalse(card.showsMergeWhenReady)
                    XCTAssertFalse(child.isMergeWhenReadyEligible)
                }
            }
        }
    }

    func testParentRollupExcludesInReviewRevisionWithOwnCard() {
        let model = makeModel()
        let parent = makeParent(status: "in_review")
        let revision = makeRevision(status: "in_review", seq: 1, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, revision]]
        XCTAssertTrue(model.workItems(in: .review).contains { $0.id == revision.id })
        let snapshot = model.workCardSnapshot(
            for: parent, column: .review, isSelected: false, isFrontierHighlighted: false,
            boardStyle: .classic, liveState: nil
        )
        XCTAssertTrue(snapshot.doneRevisions.isEmpty)
    }

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

    func testInReviewRevisionVisibleInReviewColumn() {
        let model = makeModel()
        let parent = makeParent(status: "in_review")
        let revision = makeRevision(status: "in_review", seq: 1, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, revision]]

        let reviewItems = model.workItems(in: .review)
        XCTAssertTrue(
            reviewItems.contains(where: { $0.id == parent.id }),
            "parent must appear in Review"
        )
        XCTAssertTrue(
            reviewItems.contains(where: { $0.id == revision.id }),
            "in_review revision must appear as a standalone card in Review"
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

    func testChoreParentedInReviewRevisionHasNoParentRollup() {
        let model = makeModel()
        let chore = makeChore(id: "chore_c19", status: "in_review")
        let revision = makeChoreParentedRevision(
            id: "task_t29", status: "in_review", seq: 1, parentID: chore.id
        )
        model.applyEventForTest(makeWorkTreeEvent(tasks: [revision], chores: [chore]))

        let snapshot = model.workCardSnapshot(for: chore, column: .review, isSelected: false, isFrontierHighlighted: false, boardStyle: .classic, liveState: nil)
        XCTAssertTrue(snapshot.doneRevisions.isEmpty)

        let reviewItems = model.workItems(in: .review)
        XCTAssertTrue(reviewItems.contains { $0.id == chore.id },
                      "parent chore must appear in Review")
        XCTAssertTrue(reviewItems.contains { $0.id == revision.id },
                       "in-review revision must appear as a standalone Review card")
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
