import XCTest
@testable import Boss

/// Regression coverage for the bug where `PlannerRunAffordance` never
/// refetches: the auto-populate Populator (design
/// auto-populate-project-tasks-on-design-pr-merge.md §"Surfacing") pushes a
/// `work_items_created` batch event on the project's product topic when it
/// stages a design's task breakdown, but the app never decoded that wire
/// type at all — so a planner run created after the board was already open
/// stayed invisible until the view remounted (app relaunch or navigation).
///
/// Fix: decode `work_items_created` into `.workItemsCreated`, and handle it
/// with a passive batch path (`handleCreatedWorkItemsBatch`) distinct from
/// the single-item `handleCreatedWorkItem` used for operator-initiated
/// creates. The batch path must NOT hijack the operator's current
/// selection/filters — unlike the single-item path, which intentionally
/// selects a freshly (manually) created item.
@MainActor
final class PlannerRunLiveRefreshTests: XCTestCase {

    func testWorkItemsCreatedBatchDoesNotStealSelection() {
        let model = makeModel()
        model.selectedProjectFilterIDs = ["proj_other"]
        model.selectedWorkCardID = "task_untouched"

        let batchTask = makeTask(id: "task_batch_1", projectID: "proj_populated")
        model.applyEventForTest(.workItemsCreated(items: [.task(batchTask)]))

        XCTAssertEqual(
            model.selectedProjectFilterIDs, ["proj_other"],
            "a background batch create must not change the operator's project filter"
        )
        XCTAssertEqual(
            model.selectedWorkCardID, "task_untouched",
            "a background batch create must not steal the operator's selected card"
        )
    }

    func testSingleWorkItemCreatedStillSelectsItsProject() {
        // Contrast case: the existing single-item path (operator-initiated
        // creates, e.g. "New Task") is UNCHANGED and still adopts the new
        // item's project as the active filter.
        let model = makeModel()
        model.selectedProjectFilterIDs = ["proj_other"]

        let task = makeTask(id: "task_single_1", projectID: "proj_new")
        model.applyEventForTest(.workItemCreated(item: .task(task)))

        XCTAssertEqual(model.selectedProjectFilterIDs, ["proj_new"])
    }

    // MARK: - Helpers

    private func makeTask(id: String, projectID: String?) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: projectID,
            kind: "task",
            name: "Populated task \(id)",
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
                createdAt: "2026-06-01T00:00:00Z",
                updatedAt: "2026-06-01T00:00:00Z"
            )
        ]
        model.selectWorkProduct("prod_test")
        return model
    }
}
