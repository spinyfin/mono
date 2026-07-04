import XCTest
@testable import Boss

/// Regression coverage for the `reveal_work_item` bug (T2189/T2143): a
/// revision that has reached `in_review`/`done` never gets a standalone
/// kanban card — it only ever surfaces as a rollup line on its PARENT's
/// card (see `ContentView`'s `inReviewRevisions` computation). Revealing
/// such a revision by its own id previously scrolled/highlighted an id
/// that no rendered card carried, so nothing visibly happened — and the
/// app-side IPC handler answered `Ok` regardless, so `bossctl reveal`
/// never surfaced the failure.
///
/// `revealCardTarget(for:)` redirects a rolled-up revision's reveal to its
/// parent's card, and reports `.unreachable` (with a reason) when there is
/// truly nothing to show — so `revealWorkCard` lands on a real card and the
/// `reveal_work_item` IPC handler can answer bossctl truthfully.
@MainActor
final class RevealRevisionRollupTests: XCTestCase {

    func testRevealOfInReviewRevisionRedirectsToBlockedParentCard() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeTask(id: "task_parent", name: "Parent", status: "blocked", blockedReason: "dependency"),
            makeTask(
                id: "task_revision",
                name: "Revision",
                status: "in_review",
                kind: "revision",
                parentTaskId: "task_parent"
            ),
        ]))

        // Precondition mirroring the reported bug: the parent is blocked for
        // a non-review reason, so it renders in Backlog — not Review/Done —
        // and the revision itself never gets a standalone card anywhere.
        XCTAssertEqual(model.effectiveBoardColumn(for: model.task(withID: "task_parent")!), .backlog)

        let outcome = model.revealCardTarget(for: "task_revision")
        XCTAssertEqual(outcome, .revealed(cardID: "task_parent"), "reveal must redirect to the parent's card")

        model.revealWorkCard("task_revision", productID: "prod_test")
        XCTAssertEqual(model.revealHighlightID, "task_parent", "highlight must land on the parent's card")
        XCTAssertEqual(model.revealScrollTarget, "task_parent", "scroll must target the parent's card")
        XCTAssertEqual(model.selectedWorkCardID, "task_parent")
    }

    func testRevealOfDoneRevisionRedirectsToParentRegardlessOfParentColumn() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeTask(id: "task_parent", name: "Parent", status: "todo"),
            makeTask(
                id: "task_revision",
                name: "Revision",
                status: "done",
                kind: "revision",
                parentTaskId: "task_parent"
            ),
        ]))

        XCTAssertEqual(model.revealCardTarget(for: "task_revision"), .revealed(cardID: "task_parent"))
    }

    func testRevealOfRevisionWithUnresolvableParentIsUnreachable() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeTask(
                id: "task_revision",
                name: "Revision",
                status: "in_review",
                kind: "revision",
                parentTaskId: "task_missing_parent"
            ),
        ]))

        guard case .unreachable(let reason) = model.revealCardTarget(for: "task_revision") else {
            return XCTFail("expected .unreachable when the revision's parent cannot be resolved")
        }
        XCTAssertTrue(reason.contains("task_revision") || reason.contains("could not be resolved"))
    }

    func testRevealOfOrdinaryTaskIsUnaffected() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeTask(id: "task_a", name: "Apple", status: "todo"),
        ]))

        XCTAssertEqual(model.revealCardTarget(for: "task_a"), .revealed(cardID: "task_a"))
    }

    func testRevealOfUnknownIdIsDeferredNotUnreachable() {
        let model = makeModel()
        // No work tree loaded for this id at all — mirrors a cross-product
        // reveal whose tree hasn't been fetched into this session yet.
        // Must NOT be reported as unreachable; the caller proceeds
        // optimistically and `pendingRevealScrollID` finishes the job once
        // the tree arrives.
        XCTAssertEqual(model.revealCardTarget(for: "task_never_seen"), .deferred)
    }

    // MARK: - Helpers

    private func makeTask(
        id: String,
        name: String,
        status: String = "todo",
        projectID: String? = "proj_test",
        kind: String = "task",
        parentTaskId: String? = nil,
        blockedReason: String? = nil
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: projectID,
            kind: kind,
            name: name,
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z",
            blockedReason: blockedReason,
            parentTaskId: parentTaskId
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
