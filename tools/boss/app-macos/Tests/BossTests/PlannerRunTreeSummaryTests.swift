import XCTest
@testable import Boss

/// The kanban planner affordance renders from the per-project
/// `PlannerRunSummary` the work tree carries, so a board refresh must not
/// fan out one `list_planner_runs` per project. Full history is fetched only
/// on demand (popover click, open inspector, post-action reload).
@MainActor
final class PlannerRunTreeSummaryTests: XCTestCase {

    // MARK: - Tree seeds the affordance

    func testWorkTreeSeedsAffordanceWithoutAnyPlannerRequest() {
        let model = makeConnectedModel()
        var requests: [String] = []
        model.engine.outboundRecorder = { requests.append($0["type"] as? String ?? "") }

        model.applyEventForTest(treeEvent(
            projects: [project("proj_1"), project("proj_2"), project("proj_3")],
            plannerRuns: [
                summary(runID: "run_a", projectID: "proj_1", outcome: "staged"),
                summary(runID: "run_b", projectID: "proj_2", outcome: "planner_failed"),
            ]
        ))

        let staged = model.plannerAffordancePresentation(forProjectID: "proj_1")
        XCTAssertEqual(staged?.runID, "run_a")
        XCTAssertEqual(staged?.systemImage, "tray.and.arrow.down.fill")
        XCTAssertEqual(staged?.tooltip, "Staged tasks ready for review")
        let failed = model.plannerAffordancePresentation(forProjectID: "proj_2")
        XCTAssertEqual(failed?.systemImage, "exclamationmark.circle")
        XCTAssertEqual(failed?.tintKind, .failed)
        XCTAssertNil(
            model.plannerAffordancePresentation(forProjectID: "proj_3"),
            "a project with no settled run shows no affordance"
        )
        XCTAssertFalse(requests.contains("list_planner_runs"))
        XCTAssertTrue(model.plannerRuns(forProjectID: "proj_1").isEmpty)
    }

    func testNextTreeReplacesAndClearsSummariesForItsProductOnly() {
        let model = makeConnectedModel()
        model.applyEventForTest(treeEvent(
            projects: [project("proj_1")],
            plannerRuns: [summary(runID: "run_a", projectID: "proj_1", outcome: "staged")]
        ))
        model.applyEventForTest(treeEvent(
            productID: "prod_2",
            projects: [project("proj_other", productID: "prod_2")],
            plannerRuns: [summary(runID: "run_z", projectID: "proj_other", outcome: "applied")]
        ))

        // Staged -> applied arrives as a changed summary.
        model.applyEventForTest(treeEvent(
            projects: [project("proj_1")],
            plannerRuns: [summary(runID: "run_a", projectID: "proj_1", outcome: "applied", updatedAt: "2000")]
        ))
        XCTAssertEqual(model.plannerAffordancePresentation(forProjectID: "proj_1")?.systemImage, "checkmark.circle")
        XCTAssertNotNil(model.plannerAffordancePresentation(forProjectID: "proj_other"))

        // Undo removes the run: the next tree has no summary for it.
        model.applyEventForTest(treeEvent(projects: [project("proj_1")], plannerRuns: []))
        XCTAssertNil(model.plannerAffordancePresentation(forProjectID: "proj_1"))
        XCTAssertNotNil(
            model.plannerAffordancePresentation(forProjectID: "proj_other"),
            "another product's tree must not touch this product's summaries and vice versa"
        )
    }

    // MARK: - No fan-out on invalidation / resync / batch pushes

    func testInvalidationResyncAndBatchPushNeverListPlannerRuns() async {
        let model = makeConnectedModel()
        let projects = (1...12).map { project("proj_\($0)") }
        model.applyEventForTest(treeEvent(
            projects: projects,
            plannerRuns: projects.map { summary(runID: "run_\($0.id)", projectID: $0.id, outcome: "applied") }
        ))
        model.applyEventForTest(.connected)
        var requests: [String] = []
        model.engine.outboundRecorder = { requests.append($0["type"] as? String ?? "") }

        model.applyEventForTest(.workInvalidated(topic: "work.product.prod_1", productId: "prod_1", itemIds: []))
        model.applyEventForTest(.resyncRequired)
        model.applyEventForTest(.workItemsCreated(items: [.project(projects[0])]))
        let settled = await model.waitForRefetchBurstsForTest()

        XCTAssertTrue(settled, "the refetch burst never fired")
        XCTAssertTrue(requests.contains("get_work_tree"), "the tree refetch is what carries planner state")
        XCTAssertFalse(
            requests.contains("list_planner_runs"),
            "board churn must not cost one list_planner_runs per project: \(requests)"
        )
        model.applyEventForTest(.disconnected)
    }

    // MARK: - History fetched on demand

    func testExplicitHistoryRequestsStillWork() {
        let model = makeConnectedModel()
        var requests: [String] = []
        model.engine.outboundRecorder = { requests.append($0["type"] as? String ?? "") }

        model.refreshPlannerRuns(projectID: "proj_1")
        model.openPlannerInspector(projectID: "proj_2")

        XCTAssertEqual(requests.filter { $0 == "list_planner_runs" }.count, 2)
        XCTAssertEqual(model.plannerInspectorProjectID, "proj_2")
    }

    func testOpenInspectorReloadsHistoryWhenItsProjectSummaryMoves() {
        let model = makeConnectedModel()
        model.applyEventForTest(treeEvent(
            projects: [project("proj_1"), project("proj_2")],
            plannerRuns: [
                summary(runID: "run_a", projectID: "proj_1", outcome: "staged"),
                summary(runID: "run_b", projectID: "proj_2", outcome: "staged"),
            ]
        ))
        model.plannerInspectorProjectID = "proj_1"
        var requests: [String] = []
        model.engine.outboundRecorder = { requests.append(($0["project_id"] as? String).map { "list:\($0)" } ?? "") }

        // proj_2 moves, the inspected proj_1 does not: nothing to reload.
        model.applyEventForTest(treeEvent(
            projects: [project("proj_1"), project("proj_2")],
            plannerRuns: [
                summary(runID: "run_a", projectID: "proj_1", outcome: "staged"),
                summary(runID: "run_b", projectID: "proj_2", outcome: "applied", updatedAt: "2000"),
            ]
        ))
        XCTAssertTrue(requests.isEmpty)

        model.applyEventForTest(treeEvent(
            projects: [project("proj_1"), project("proj_2")],
            plannerRuns: [
                summary(runID: "run_a", projectID: "proj_1", outcome: "applied", updatedAt: "2000"),
                summary(runID: "run_b", projectID: "proj_2", outcome: "applied", updatedAt: "2000"),
            ]
        ))
        XCTAssertEqual(requests, ["list:proj_1"])
    }

    // MARK: - Popover freshness against the tree summary

    func testPopoverRowIsHeldBackWhileCachedRowPredatesTheSummary() {
        let model = makeConnectedModel()
        model.applyEventForTest(treeEvent(
            projects: [project("proj_1")],
            plannerRuns: [summary(runID: "run_a", projectID: "proj_1", outcome: "applied", updatedAt: "2000")]
        ))

        XCTAssertNil(
            model.plannerRunForAffordancePopover(forProjectID: "proj_1"),
            "nothing cached: the popover must wait for a fetch"
        )

        model.plannerRunsByProjectID["proj_1"] = [run(id: "run_a", projectID: "proj_1", outcome: "staged", updatedAt: "1000")]
        XCTAssertNil(
            model.plannerRunForAffordancePopover(forProjectID: "proj_1"),
            "a row cached as staged must not offer Release for a run the tree says is applied"
        )

        model.plannerRunsByProjectID["proj_1"] = [run(id: "run_a", projectID: "proj_1", outcome: "applied", updatedAt: "2000")]
        XCTAssertEqual(model.plannerRunForAffordancePopover(forProjectID: "proj_1")?.outcome, "applied")

        model.plannerRunsByProjectID["proj_1"] = [run(id: "run_a", projectID: "proj_1", outcome: "applied", updatedAt: "2500")]
        XCTAssertNotNil(
            model.plannerRunForAffordancePopover(forProjectID: "proj_1"),
            "a cached row newer than the tree (action reply raced the tree) is still current"
        )
    }

    // MARK: - Wire

    func testWireDecodesPlannerRunSummariesFromFullTreePayload() async throws {
        let model = makeModel()
        let tree: [String: Any] = [
            "type": "work_tree",
            "product": ["id": "prod_1", "name": "Fixture", "slug": "fixture", "description": "",
                        "status": "active", "created_at": "1", "updated_at": "1"],
            "planner_runs": [[
                "run_id": "run_wire", "project_id": "proj_1", "outcome": "staged",
                "created_at": "10", "updated_at": "20",
            ]],
        ]
        let applied = expectation(description: "tree decoded")
        let client = EngineClient(socketPath: "/tmp/boss-decode-\(UUID().uuidString).sock")
        client.onEvent = { event in
            if case .workTree = event {
                model.applyEventForTest(event)
                applied.fulfill()
            }
        }
        let data = try JSONSerialization.data(withJSONObject: ["payload": tree])
        client.consumeLineForTesting(try XCTUnwrap(String(data: data, encoding: .utf8)))
        await fulfillment(of: [applied], timeout: 3)

        XCTAssertEqual(
            model.plannerRunSummaryByProjectID["proj_1"],
            PlannerRunSummary(runID: "run_wire", projectID: "proj_1", outcome: "staged", createdAt: "10", updatedAt: "20")
        )
    }

    // MARK: - Fixtures

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-planner-tree-\(UUID().uuidString).sock")
    }

    private func makeConnectedModel() -> ChatViewModel {
        let model = makeModel()
        model.engine.simulateConnectedForTesting = true
        model.isConnected = true
        return model
    }

    private func product(_ id: String) -> WorkProduct {
        WorkProduct(id: id, name: id, slug: id, description: "", repoRemoteURL: nil,
                    status: "active", createdAt: "1", updatedAt: "1")
    }

    private func project(_ id: String, productID: String = "prod_1") -> WorkProject {
        WorkProject(id: id, productID: productID, name: id, slug: id, description: "", goal: "",
                    status: "active", priority: "medium", createdAt: "1", updatedAt: "1")
    }

    private func treeEvent(
        productID: String = "prod_1",
        projects: [WorkProject],
        plannerRuns: [PlannerRunSummary]
    ) -> EngineEvent {
        .workTree(product: product(productID), projects: projects, tasks: [], chores: [],
                  taskRuntimes: [], dependencies: [], ideas: [], plannerRuns: plannerRuns)
    }

    private func summary(runID: String, projectID: String, outcome: String, updatedAt: String = "1000") -> PlannerRunSummary {
        PlannerRunSummary(runID: runID, projectID: projectID, outcome: outcome, createdAt: "1000", updatedAt: updatedAt)
    }

    private func run(id: String, projectID: String, outcome: String, updatedAt: String) -> PlannerRun {
        PlannerRun(id: id, projectID: projectID, productID: "prod_1", designTaskID: nil, caller: "merge_trigger",
                   docRef: nil, model: nil, inputSummary: nil, rawOutput: nil, effortAudit: nil, notes: nil,
                   outcome: outcome, resultSummary: nil, createdAt: "1000", updatedAt: updatedAt)
    }
}
