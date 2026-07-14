import XCTest
@testable import Boss

/// Covers the "Merging" kanban section: an `in_review` task whose PR is
/// either in GitHub's merge queue or has Merge When Ready armed
/// (`mergeQueueState` non-nil) routes into the Done column's collapsible
/// "Merging" section instead of Review, ordered by the engine-computed
/// `section_order`. A task whose PR drops out of the queue/auto-merge
/// without merging needs no special transition — `mergeQueueState` simply
/// reverts to `nil` and `boardColumn` routes it back to Review on its own.
@MainActor
final class MergingSectionKanbanTests: XCTestCase {

    // MARK: boardColumn / isInMergingSection routing

    func testInReviewWithNoMergeQueueStateRoutesToReview() {
        let task = makeTask(status: "in_review", mergeQueueState: nil)
        XCTAssertFalse(task.isInMergingSection)
        XCTAssertEqual(task.boardColumn, .review)
    }

    func testQueuedInReviewRoutesToDoneMergingSection() {
        let task = makeTask(status: "in_review", mergeQueueState: "queued")
        XCTAssertTrue(task.isInMergingSection)
        XCTAssertEqual(task.boardColumn, .done)
    }

    func testAutoMergeEnabledInReviewRoutesToDoneMergingSection() {
        let task = makeTask(status: "in_review", mergeQueueState: "auto_merge_enabled")
        XCTAssertTrue(task.isInMergingSection)
        XCTAssertEqual(task.boardColumn, .done)
    }

    /// A stale `mergeQueueState` on an already-`done` row (the merge poller
    /// only writes poll state for `Open` PRs, so the column isn't cleared
    /// on merge) must not fool `isInMergingSection`/`boardColumn` — `done`
    /// always wins.
    func testDoneStatusIgnoresStaleMergeQueueState() {
        let task = makeTask(status: "done", mergeQueueState: "queued")
        XCTAssertEqual(task.boardColumn, .done)
    }

    /// Simulates the drop-out case: the PR left the queue/auto-merge
    /// without merging. `status` never changed (it was `in_review` the
    /// whole time), so once `mergeQueueState` reverts to `nil` the card
    /// falls straight back to Review with no separate transition needed.
    func testDroppingMergeQueueStateRoutesBackToReview() {
        let stillMerging = makeTask(status: "in_review", mergeQueueState: "queued")
        XCTAssertEqual(stillMerging.boardColumn, .done)

        var droppedOut = stillMerging
        droppedOut.mergeQueueState = nil
        droppedOut.mergeQueueDetail = nil
        XCTAssertEqual(droppedOut.boardColumn, .review)
    }

    // MARK: ChatViewModel.mergingSection ordering

    func testMergingSectionReturnsNilForEmptyItems() {
        XCTAssertNil(ChatViewModel.mergingSection(items: []))
    }

    func testMergingSectionOrdersByEngineSectionOrderAscending() {
        let third = makeTask(id: "task_c", status: "in_review", mergeQueueState: "auto_merge_enabled", sectionOrder: 1_000_000)
        let first = makeTask(id: "task_a", status: "in_review", mergeQueueState: "queued", sectionOrder: 1)
        let second = makeTask(id: "task_b", status: "in_review", mergeQueueState: "queued", sectionOrder: 2)

        let section = ChatViewModel.mergingSection(items: [third, first, second])
        XCTAssertEqual(section?.title, "Merging")
        XCTAssertTrue(section?.isCollapsible ?? false)
        XCTAssertEqual(section?.items.map(\.id), ["task_a", "task_b", "task_c"])
    }

    func testMergingSectionSortsMissingSectionOrderLast() {
        let queued = makeTask(id: "task_queued", status: "in_review", mergeQueueState: "queued", sectionOrder: 1)
        var malformed = makeTask(id: "task_malformed", status: "in_review", mergeQueueState: "auto_merge_enabled", sectionOrder: 0)
        // Simulate a legacy/malformed payload with no parseable section_order.
        malformed.mergeQueueDetail = "not-json"

        let section = ChatViewModel.mergingSection(items: [malformed, queued])
        XCTAssertEqual(section?.items.map(\.id), ["task_queued", "task_malformed"])
    }

    // MARK: workSections(in: .done) integration

    func testMergingSectionAppearsAboveTodayAndDisappearsWhenEmpty() {
        let model = makeModel()
        let queued = makeTask(id: "task_queued", status: "in_review", mergeQueueState: "queued", sectionOrder: 1)
        let done = makeTask(id: "task_done", status: "done", mergeQueueState: nil)
        model.choresByProductID = ["prod_test": [queued, done]]

        let sections = model.workSections(in: .done)
        XCTAssertEqual(sections.first?.title, "Merging")
        XCTAssertEqual(sections.first?.items.map(\.id), ["task_queued"])
        XCTAssertFalse(
            sections.dropFirst().contains { $0.items.contains { $0.id == "task_queued" } },
            "the merging task must not also appear in a recency bucket"
        )

        model.choresByProductID = ["prod_test": [done]]
        let sectionsWithoutMerging = model.workSections(in: .done)
        XCTAssertFalse(
            sectionsWithoutMerging.contains { $0.title == "Merging" },
            "Merging section must not render at all once it has no tasks"
        )
    }

    // MARK: - Helpers

    private func makeTask(
        id: String = "task_\(UUID().uuidString)",
        status: String,
        mergeQueueState: String?,
        sectionOrder: Int64? = nil
    ) -> WorkTask {
        var task = WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Test item",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/foo/bar/pull/1",
            deletedAt: nil,
            createdAt: "2026-05-13T00:00:00Z",
            updatedAt: "2026-05-13T00:00:00Z",
            autostart: false
        )
        task.mergeQueueState = mergeQueueState
        if mergeQueueState != nil {
            let order = sectionOrder ?? 0
            task.mergeQueueDetail = #"{"position":null,"state":null,"enqueued_at":null,"section_order":\#(order)}"#
        }
        return task
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
                createdAt: "2026-05-13T00:00:00Z",
                updatedAt: "2026-05-13T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = "prod_test"
        return model
    }
}
