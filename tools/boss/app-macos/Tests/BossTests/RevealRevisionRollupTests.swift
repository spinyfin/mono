import XCTest
@testable import Boss

/// Review revisions reveal themselves; completed revisions reveal the parent rollup.
@MainActor
final class RevealRevisionRollupTests: XCTestCase {

    func testRevealOfInReviewRevisionTargetsItsOwnCard() {
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

        let outcome = model.revealCardTarget(for: "task_revision")
        XCTAssertEqual(outcome, .revealed(cardID: "task_revision"), "reveal must redirect to the revision card")

        model.revealWorkCard("task_revision", productID: "prod_test")
        XCTAssertNil(model.revealHighlightID, "do not flash before the card is visible")
        XCTAssertEqual(model.revealScrollTarget, "task_revision", "scroll must target the revision card")
        model.confirmReveal(cardID: "task_revision", generation: model.revealGeneration)
        XCTAssertEqual(model.revealHighlightID, "task_revision", "highlight must land on the revision card")
        XCTAssertEqual(model.selectedWorkCardID, "task_revision")
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
                status: "done",
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

    func testUnreachableRevealClearsThePreviousCardsFlash() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeTask(id: "previous", name: "Previous", status: "active"),
            makeTask(id: "unreachable", name: "Revision", status: "done", kind: "revision"),
        ]))
        model.revealWorkCard("previous", productID: "prod_test")
        model.confirmReveal(cardID: "previous", generation: model.revealGeneration)
        XCTAssertEqual(model.revealHighlightID, "previous")

        guard case .unreachable = model.revealWorkCard("unreachable", productID: "prod_test") else {
            return XCTFail("a revision without its parent has no host card")
        }
        XCTAssertNil(model.revealHighlightID, "an unsuccessful reveal must not leave an unrelated card flashing")
        XCTAssertNil(model.revealScrollTarget)
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
                createdAt: "2026-05-26T00:00:00Z",
                updatedAt: "2026-05-26T00:00:00Z"
            )
        ]
        model.selectWorkProduct("prod_test")
        return model
    }
}
