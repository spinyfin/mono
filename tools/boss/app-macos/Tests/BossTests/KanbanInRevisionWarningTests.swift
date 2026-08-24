import XCTest
@testable import Boss

/// Covers the "in revision" warning indicator on kanban PR cards.
///
/// When a chain-root task (the one carrying `prURL`) has at least one
/// descendant revision whose status is `todo` or `active`, the engine
/// sets `has_in_progress_revision = true` on the root's work-tree row.
/// The macOS app surfaces this as an orange "in revision" badge next to
/// the PR link chip on the card.
///
/// These tests pin the `hasInProgressRevision` field behaviour on the
/// Swift side: default, parse round-trip, and model-level flag.
@MainActor
final class KanbanInRevisionWarningTests: XCTestCase {

    // MARK: WorkTask field defaults

    func testHasInProgressRevisionDefaultsFalse() {
        let task = makeTaskWithPR(prURL: "https://github.com/org/repo/pull/1")
        XCTAssertFalse(task.hasInProgressRevision,
                       "hasInProgressRevision must default to false")
    }

    func testHasInProgressRevisionTruePreserved() {
        var task = makeTaskWithPR(prURL: "https://github.com/org/repo/pull/1")
        task.hasInProgressRevision = true
        XCTAssertTrue(task.hasInProgressRevision,
                      "hasInProgressRevision must be preserved when set to true")
    }

    // MARK: Show / hide logic (mirroring engine signal rules)

    /// A task with `hasInProgressRevision = true` should show the warning.
    func testWarningShownWhenFlagSet() {
        var task = makeTaskWithPR(prURL: "https://github.com/org/repo/pull/2")
        task.hasInProgressRevision = true
        XCTAssertTrue(task.hasInProgressRevision,
                      "flag must be true — view layer should render PrInRevisionIndicator")
    }

    /// A task with `hasInProgressRevision = false` (the default) must not show the warning.
    func testWarningHiddenWhenFlagClear() {
        let task = makeTaskWithPR(prURL: "https://github.com/org/repo/pull/3")
        XCTAssertFalse(task.hasInProgressRevision,
                       "flag must be false — view layer must NOT render PrInRevisionIndicator")
    }

    /// A task without a PR URL has `hasInProgressRevision` irrelevant —
    /// the PR row is not rendered, so the indicator can never appear.
    func testNoPRURLTaskFlagIrrelevant() {
        var task = makeTask(status: "active")
        task.hasInProgressRevision = true
        XCTAssertNil(task.prURL,
                     "sanity: task must have no PR URL so the PR row is not rendered")
    }

    // MARK: mostRecentActiveRevision(forParentID:)

    /// Multiple open revisions on the same parent: the highest
    /// `revisionSeq` wins, mirroring `setRevisionBadgeHover`'s membership.
    func testMostRecentActiveRevisionPicksHighestSeq() {
        let model = makeModel()
        let parent = makeParent(status: "in_review")
        let r1 = makeRevision(id: "task_r1", status: "todo", seq: 1, parentID: parent.id)
        let r2 = makeRevision(id: "task_r2", status: "active", seq: 5, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, r1, r2]]

        XCTAssertEqual(model.mostRecentActiveRevision(forParentID: parent.id)?.id, r2.id,
                       "the revision with the higher revisionSeq must win")
    }

    /// A revision with a `nil` revisionSeq must not crash the tiebreak and
    /// must still be resolvable when it's the only candidate.
    func testMostRecentActiveRevisionHandlesNilSeq() {
        let model = makeModel()
        let parent = makeParent(status: "in_review")
        let revision = makeRevision(id: "task_r1", status: "todo", seq: nil, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, revision]]

        XCTAssertEqual(model.mostRecentActiveRevision(forParentID: parent.id)?.id, revision.id)
    }

    /// A chore-parented (product-level) revision must resolve via
    /// `productLevelRevisionsByProductID`, not just `tasksByProjectID`.
    func testMostRecentActiveRevisionFindsProductLevelRevision() {
        let model = makeModel()
        let chore = makeChore(id: "chore_1", status: "active")
        let revision = makeRevision(id: "task_r1", status: "todo", seq: 1, parentID: chore.id, projectID: nil)
        model.choresByProductID = ["prod_test": [chore]]
        model.productLevelRevisionsByProductID = ["prod_test": [revision]]

        XCTAssertEqual(model.mostRecentActiveRevision(forParentID: chore.id)?.id, revision.id)
    }

    /// `done` / `in_review` revisions are not in-progress and must be
    /// excluded from the resolver, same as the hover-highlight rule.
    func testMostRecentActiveRevisionExcludesDoneAndInReview() {
        let model = makeModel()
        let parent = makeParent(status: "in_review")
        let done = makeRevision(id: "task_done", status: "done", seq: 2, parentID: parent.id)
        let inReview = makeRevision(id: "task_in_review", status: "in_review", seq: 3, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, done, inReview]]

        XCTAssertNil(model.mostRecentActiveRevision(forParentID: parent.id),
                    "done/in_review revisions must not resolve as the in-progress revision")
    }

    /// `applyIncrementalTaskUpdate` treats a `workItemUpdated` payload as a
    /// complete row. A wire object that omits `has_in_progress_revision`
    /// (the pre-fix single-row shape) therefore clears the "In revision"
    /// badge — which is why the engine must send the derived field.
    func testWorkItemUpdatedPayloadMissingKeyClearsInRevisionBadge() {
        let model = makeModel()
        var seeded = makeTaskWithPR(id: "chore_in_rev", prURL: "https://github.com/org/repo/pull/9")
        seeded.hasInProgressRevision = true
        model.applyEventForTest(makeWorkTreeEvent(chores: [seeded]))

        let before = model.choresByProductID["prod_test"]?.first { $0.id == seeded.id }
        XCTAssertEqual(before?.hasInProgressRevision, true)
        XCTAssertTrue(
            footerSlice(for: before!).hasInProgressRevision,
            "seeded card must show the In revision badge"
        )

        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let parsed = client.parseTask(wirePayload(for: seeded, includeHasInProgressRevision: false))
        XCTAssertEqual(parsed?.hasInProgressRevision, false,
                       "omitting the key must decode as false — the client does not merge")
        model.applyEventForTest(.workItemUpdated(item: .chore(parsed!)))

        let after = model.choresByProductID["prod_test"]?.first { $0.id == seeded.id }
        XCTAssertEqual(after?.hasInProgressRevision, false)
        XCTAssertFalse(
            footerSlice(for: after!).hasInProgressRevision,
            "a de-projected workItemUpdated must hide the In revision badge"
        )
    }

    /// A `workItemUpdated` payload that carries the derived projections must
    /// keep the In revision badge through `applyIncrementalTaskUpdate`'s
    /// evict-and-reinsert.
    func testWorkItemUpdatedCompletePayloadKeepsInRevisionBadge() {
        let model = makeModel()
        var seeded = makeTaskWithPR(id: "chore_in_rev", prURL: "https://github.com/org/repo/pull/9")
        seeded.hasInProgressRevision = true
        seeded.hasAttachments = true
        seeded.readyForReview = false
        model.applyEventForTest(makeWorkTreeEvent(chores: [seeded]))

        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let parsed = client.parseTask(wirePayload(for: seeded, includeHasInProgressRevision: true))
        XCTAssertEqual(parsed?.hasInProgressRevision, true)
        XCTAssertEqual(parsed?.hasAttachments, true)
        model.applyEventForTest(.workItemUpdated(item: .chore(parsed!)))

        let after = model.choresByProductID["prod_test"]?.first { $0.id == seeded.id }
        XCTAssertEqual(after?.hasInProgressRevision, true)
        XCTAssertEqual(after?.hasAttachments, true)
        XCTAssertTrue(
            footerSlice(for: after!).hasInProgressRevision,
            "a complete workItemUpdated must keep the In revision badge"
        )
    }

    /// A revision-of-revision (nested chain: task -> R1(done) -> R2(active))
    /// must still resolve against the chain ROOT, mirroring the engine's
    /// `attach_in_progress_revision_flag` walk — the badge is set on the
    /// chain root for any in-progress descendant, not just a direct child.
    func testMostRecentActiveRevisionWalksNestedRevisionChain() {
        let model = makeModel()
        let parent = makeParent(status: "in_review")
        let r1 = makeRevision(id: "task_r1", status: "done", seq: 1, parentID: parent.id)
        let r2 = makeRevision(id: "task_r2", status: "active", seq: 2, parentID: r1.id)
        model.tasksByProjectID = ["proj_1": [parent, r1, r2]]

        XCTAssertEqual(model.mostRecentActiveRevision(forParentID: parent.id)?.id, r2.id,
                       "a revision-of-revision must resolve to the chain root's badge, not require a direct parentTaskId match")
    }

    // MARK: - Helpers

    private func makeParent(status: String) -> WorkTask {
        WorkTask(
            id: "task_parent_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: "proj_1",
            kind: "chore",
            name: "Parent task",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: status == "in_review" ? "https://github.com/org/repo/pull/1" : nil,
            deletedAt: nil,
            createdAt: "2026-05-28T00:00:00Z",
            updatedAt: "2026-05-28T00:00:00Z"
        )
    }

    private func makeChore(id: String, status: String) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Chore \(id)",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-28T00:00:00Z",
            updatedAt: "2026-05-28T00:00:00Z"
        )
    }

    private func makeRevision(
        id: String,
        status: String,
        seq: Int?,
        parentID: String,
        projectID: String? = "proj_1"
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: projectID,
            kind: "revision",
            name: "Revision \(id)",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-28T00:00:00Z",
            updatedAt: "2026-05-28T00:00:00Z",
            parentTaskId: parentID,
            revisionSeq: seq
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
                createdAt: "2026-05-28T00:00:00Z",
                updatedAt: "2026-05-28T00:00:00Z"
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
                    createdAt: "2026-05-28T00:00:00Z",
                    updatedAt: "2026-05-28T00:00:00Z"
                )
            ]
        ]
        model.selectedWorkProductID = "prod_test"
        model.includeChores = true
        return model
    }

    private func makeTaskWithPR(id: String? = nil, prURL: String) -> WorkTask {
        WorkTask(
            id: id ?? "task_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "PR task",
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: prURL,
            deletedAt: nil,
            createdAt: "2026-05-28T00:00:00Z",
            updatedAt: "2026-05-28T00:00:00Z"
        )
    }

    private func footerSlice(for task: WorkTask) -> WorkBoardCardFooterSlice {
        WorkBoardCardFooterSlice(
            snapshot: WorkCardSnapshot.build(
                task: task,
                context: WorkCardSnapshotContext(column: .review)
            )
        )
    }

    private func makeWorkTreeEvent(chores: [WorkTask]) -> EngineEvent {
        .workTree(
            product: WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-28T00:00:00Z",
                updatedAt: "2026-05-28T00:00:00Z"
            ),
            projects: [],
            tasks: [],
            chores: chores,
            taskRuntimes: [],
            dependencies: []
        )
    }

    private func wirePayload(for task: WorkTask, includeHasInProgressRevision: Bool) -> [String: Any] {
        var payload: [String: Any] = [
            "id": task.id,
            "product_id": task.productID,
            "kind": task.kind,
            "name": task.name,
            "description": task.description,
            "status": task.status,
            "created_at": task.createdAt,
            "updated_at": task.updatedAt,
        ]
        if let prURL = task.prURL {
            payload["pr_url"] = prURL
        }
        if includeHasInProgressRevision {
            payload["has_in_progress_revision"] = task.hasInProgressRevision
            payload["has_attachments"] = task.hasAttachments
            payload["ready_for_review"] = task.readyForReview
        }
        return payload
    }

    private func makeTask(status: String) -> WorkTask {
        WorkTask(
            id: "task_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: "proj_1",
            kind: "chore",
            name: "No PR task",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-28T00:00:00Z",
            updatedAt: "2026-05-28T00:00:00Z"
        )
    }
}
