import XCTest
@testable import Boss

/// Regression coverage for issue #1249: `bossctl reveal <id>` scrolls the
/// kanban to a card and highlights it, but if an active board filter
/// excludes the target the card stays hidden and the scroll lands on
/// nothing — `reveal` reports success while revealing nothing.
///
/// `revealWorkCard` must reset every narrowing filter (search query,
/// blocked-only, chores-only, hidden chores, project filter) before
/// scrolling so the revealed card is guaranteed visible.
@MainActor
final class RevealClearsFiltersTests: XCTestCase {

    func testRevealClearsSearchTextThatHidesTarget() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeTask(id: "task_a", name: "Apple"),
            makeTask(id: "task_b", name: "Banana"),
        ]))
        // A stale search filter that excludes the reveal target.
        model.workSearchText = "Banana"
        XCTAssertFalse(
            model.visibleWorkItems.contains { $0.id == "task_a" },
            "precondition: search filter hides the target card"
        )

        model.revealWorkCard("task_a", productID: "prod_test")

        XCTAssertEqual(model.workSearchText, "", "reveal must clear the search filter")
        XCTAssertTrue(
            model.visibleWorkItems.contains { $0.id == "task_a" },
            "revealed card must be visible after the search filter is cleared"
        )
        XCTAssertEqual(model.revealScrollTarget, "task_a", "reveal must queue a scroll to the card")
    }

    func testRevealClearsBlockedOnlyFilter() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeTask(id: "task_a", name: "Apple", status: "todo"),
        ]))
        model.showBlockedOnly = true
        XCTAssertFalse(model.visibleWorkItems.contains { $0.id == "task_a" })

        model.revealWorkCard("task_a", productID: "prod_test")

        XCTAssertFalse(model.showBlockedOnly, "reveal must clear the blocked-only filter")
        XCTAssertTrue(model.visibleWorkItems.contains { $0.id == "task_a" })
    }

    func testRevealClearsChoresOnlyFilterToShowATask() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(
            tasks: [makeTask(id: "task_a", name: "Apple", projectID: "proj_test")],
            chores: [makeTask(id: "chore_x", name: "Chore", projectID: nil)]
        ))
        // Chores-only hides the project task we want to reveal.
        model.filterToChoresOnly = true
        XCTAssertFalse(model.visibleWorkItems.contains { $0.id == "task_a" })

        model.revealWorkCard("task_a", productID: "prod_test")

        XCTAssertFalse(model.filterToChoresOnly, "reveal must clear the chores-only filter")
        XCTAssertTrue(model.visibleWorkItems.contains { $0.id == "task_a" })
    }

    func testRevealRestoresHiddenChoresToShowAChore() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(
            chores: [makeTask(id: "chore_x", name: "Chore", projectID: nil)]
        ))
        // Chores hidden — a chore reveal target would stay invisible.
        model.includeChores = false
        XCTAssertFalse(model.visibleWorkItems.contains { $0.id == "chore_x" })

        model.revealWorkCard("chore_x", productID: "prod_test")

        XCTAssertTrue(model.includeChores, "reveal must re-enable chores so a chore target shows")
        XCTAssertTrue(model.visibleWorkItems.contains { $0.id == "chore_x" })
    }

    func testRevealClearsProjectFilter() {
        let model = makeModel()
        model.selectedProjectFilterIDs = ["proj_other"]

        model.revealWorkCard("task_a", productID: "prod_test")

        XCTAssertTrue(model.selectedProjectFilterIDs.isEmpty, "reveal must clear the project filter")
    }

    func testEveryLifecycleStateRequiresExactViewportConfirmation() {
        for status in ["todo", "active", "blocked", "in_review", "done", "archived"] {
            let model = makeModel()
            var task = makeTask(id: "target", name: "Target", status: status)
            task.prURL = "https://github.com/example/repo/pull/42"
            model.applyEventForTest(makeWorkTreeEvent(tasks: [task]))
            var result: EngineRevealResult?
            XCTAssertEqual(
                model.revealWorkCard("target", productID: "prod_test") { result = $0 },
                .revealed(cardID: "target"), status
            )
            XCTAssertNil(result, status)
            XCTAssertNil(model.revealHighlightID, status)
            model.confirmReveal(cardID: "target", generation: model.revealGeneration)
            guard case .success = result else { return XCTFail("expected visible \(status) card") }
            XCTAssertEqual(model.revealHighlightID, "target", status)
        }
    }

    func testNoVisibleCardNeverReportsSuccess() async throws {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [makeTask(id: "done", name: "Done", status: "done")]))
        var result: EngineRevealResult?
        model.revealWorkCard("done", productID: "prod_test") { result = $0 }
        XCTAssertNil(result, "scheduling a scroll is not success")
        XCTAssertNil(model.revealHighlightID)
        try await Task.sleep(for: .milliseconds(3200))
        guard case .failure(.internalFailure(let reason)) = result else {
            return XCTFail("a card with no mounted viewport must fail")
        }
        XCTAssertTrue(reason.contains("did not become visible"))
        XCTAssertNil(model.revealScrollTarget)
        XCTAssertNil(model.revealHighlightID)
    }

    func testOnlyCurrentExactCardCanAcknowledgeReveal() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeTask(id: "first", name: "First"), makeTask(id: "second", name: "Second"),
        ]))
        var first: EngineRevealResult?
        var second: EngineRevealResult?
        model.revealWorkCard("first", productID: "prod_test") { first = $0 }
        let stale = model.revealGeneration
        model.revealWorkCard("second", productID: "prod_test") { second = $0 }
        guard case .failure(.internalFailure(let reason)) = first else {
            return XCTFail("superseded request must fail")
        }
        XCTAssertTrue(reason.contains("superseded"))
        model.confirmReveal(cardID: "first", generation: stale)
        model.confirmReveal(cardID: "first", generation: model.revealGeneration)
        model.confirmReveal(cardID: "second", generation: stale)
        XCTAssertNil(second)
        XCTAssertNil(model.revealHighlightID)
        model.confirmReveal(cardID: "second", generation: model.revealGeneration)
        guard case .success = second else { return XCTFail("visible exact target must succeed") }
        XCTAssertEqual(model.revealHighlightID, "second")
    }

    func testDeferredRevealResolvesAfterTreeAndStillWaitsForViewport() {
        let model = makeModel()
        var result: EngineRevealResult?
        model.revealWorkCard("later", productID: "prod_test") { result = $0 }
        XCTAssertNil(result)
        model.applyEventForTest(makeWorkTreeEvent(tasks: [makeTask(id: "later", name: "Later")]))
        XCTAssertEqual(model.revealScrollTarget, "later")
        XCTAssertNil(result)
        model.confirmReveal(cardID: "later", generation: model.revealGeneration)
        guard case .success = result else { return XCTFail("expected confirmed success") }
    }

    func testLoadedTreeMissingDeferredTargetFailsWithReason() {
        let model = makeModel()
        var result: EngineRevealResult?
        model.revealWorkCard("missing", productID: "prod_test") { result = $0 }
        model.applyEventForTest(makeWorkTreeEvent())
        guard case .failure(.internalFailure(let reason)) = result else {
            return XCTFail("a loaded tree without the target must fail")
        }
        XCTAssertTrue(reason.contains("missing"))
        XCTAssertTrue(reason.contains("no card"))
    }

    // MARK: - Helpers

    private func makeTask(
        id: String,
        name: String,
        status: String = "todo",
        projectID: String? = "proj_test"
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: projectID,
            kind: "task",
            name: name,
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z"
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
