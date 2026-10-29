import XCTest
@testable import Boss

/// Doing's Queued section renders the engine's dispatch order, decisions live
/// in their own Needs Attention section, and only the first runnable
/// main-pool card is marked next up.
@MainActor
final class QueuedDispatchOrderTests: XCTestCase {
    private func card(_ id: String, createdAt: String = "2026-10-01") -> WorkTask {
        WorkTask(
            id: id, productID: "product", projectID: nil, kind: "chore",
            name: id, description: "", status: "active", priority: "medium", ordinal: nil,
            prURL: nil, deletedAt: nil, createdAt: createdAt, updatedAt: createdAt
        )
    }

    private func decision(_ id: String, askedAt: String) -> WorkTask {
        var task = card(id)
        task.status = "blocked"
        task.blockedReason = "awaiting_operator_answer"
        task.operatorQuestion = OperatorQuestion(
            id: "oq_\(id)", text: "Proceed?", answerType: .yesNo, explanation: "",
            askedAt: askedAt, executionID: "exec_\(id)"
        )
        return task
    }

    func testQueuedFollowsEnginePositionNotTaskAge() {
        // Board order (oldest task first) is the reverse of the dispatch order,
        // as when a retried task's new execution is newer than younger tasks.
        let items = [card("oldest", createdAt: "2026-10-01"), card("middle", createdAt: "2026-10-02"),
                     card("newest", createdAt: "2026-10-03")]
        let dispatch = [
            "oldest": QueuedDispatch(pool: .main, position: 2),
            "middle": QueuedDispatch(pool: .main, position: 1),
            "newest": QueuedDispatch(pool: .main, position: 0, nextUp: true),
        ]

        let section = ChatViewModel.queuedSection(items: items, dispatch: dispatch)

        XCTAssertEqual(section?.items.map(\.id), ["newest", "middle", "oldest"])
        XCTAssertNil(section?.countLabel, "Queued shows its plain queued count")
    }

    func testCardsTheDispatcherWontTakeSortBelowRunnableOnesInBoardOrder() {
        let items = [
            card("no_execution"), card("paused"), card("runnable_late"), card("dependent"), card("runnable_first"),
        ]
        let dispatch = [
            "paused": QueuedDispatch(pool: .automation, position: nil),
            "runnable_late": QueuedDispatch(pool: .main, position: 5),
            "runnable_first": QueuedDispatch(pool: .review, position: 1),
        ]

        let section = ChatViewModel.queuedSection(items: items, dispatch: dispatch)

        XCTAssertEqual(
            section?.items.map(\.id),
            ["runnable_first", "runnable_late", "no_execution", "paused", "dependent"]
        )
    }

    func testWithoutAnyEnginePayloadQueuedKeepsBoardOrder() {
        let items = [card("a"), card("b"), card("c")]
        XCTAssertEqual(ChatViewModel.queuedSection(items: items)?.items.map(\.id), ["a", "b", "c"])
    }

    func testDecisionsLeaveQueuedForNeedsAttentionAboveIt() {
        let items = [
            card("queued_a"), decision("late_question", askedAt: "1790000300"),
            decision("early_question", askedAt: "1790000100"), card("coding"),
        ]
        let sections = ChatViewModel.doingSections(
            items: items, membership: ["coding": .coding],
            dispatch: ["queued_a": QueuedDispatch(pool: .main, position: 0, nextUp: true)]
        )

        XCTAssertEqual(sections.map(\.title), ["Needs Attention", "Coding", "Queued"])
        XCTAssertEqual(sections.map(\.id), ["doing-needs-attention", "doing-coding", "doing-queued"])
        XCTAssertEqual(sections[0].items.map(\.id), ["early_question", "late_question"])
        XCTAssertEqual(sections[0].countLabel, "2 decisions waiting")
        XCTAssertEqual(sections[2].items.map(\.id), ["queued_a"])
        XCTAssertNil(sections[2].countLabel)
    }

    func testNeedsAttentionIsHiddenWhenNoCardAwaitsAnAnswer() {
        let sections = ChatViewModel.doingSections(items: [card("a")], membership: [:])
        XCTAssertEqual(sections.map(\.title), ["Queued"])
        let onlyDecision = ChatViewModel.doingSections(items: [decision("q", askedAt: "1790000100")], membership: [:])
        XCTAssertEqual(onlyDecision.map(\.title), ["Needs Attention"], "an empty Queued section is omitted too")
    }

    func testSnapshotPushReordersQueuedAndOnlyPublishesChanges() {
        let model = ChatViewModel(socketPath: "/tmp/queued-order-\(UUID().uuidString).sock")
        model.selectedWorkProductID = "product"
        model.choresByProductID = ["product": [card("first", createdAt: "2026-10-01"), card("second", createdAt: "2026-10-02")]]
        XCTAssertEqual(model.workSections(in: .doing).flatMap(\.items).map(\.id), ["first", "second"])
        var publishes = 0
        let subscription = model.$queuedDispatchByID.dropFirst().sink { _ in publishes += 1 }
        let reordered = [
            "second": QueuedDispatch(pool: .main, position: 0, nextUp: true),
            "first": QueuedDispatch(pool: .main, position: 1),
        ]

        model.applyEventForTest(.workerLiveStatesList(states: [], queuedDispatch: reordered))
        model.applyEventForTest(.workerLiveStatesList(states: [], queuedDispatch: reordered))

        XCTAssertEqual(model.workSections(in: .doing).flatMap(\.items).map(\.id), ["second", "first"])
        XCTAssertEqual(publishes, 1)
        withExtendedLifetime(subscription) {}
    }

    func testOnlyTheNextUpCardCarriesTheMarkerAndKeepsItsWaitReason() {
        let runtime = WorkTaskRuntime(
            workItemID: "next", executionStatus: "ready", runStatus: nil, executionID: "exec_next",
            dispatchRetryAt: nil, dispatchWaitReason: "pool_exhausted", dispatchWaitSince: nil
        )
        let plain = WorkTaskRuntime(
            workItemID: "later", executionStatus: "ready", runStatus: nil, executionID: "exec_later",
            dispatchRetryAt: nil, dispatchWaitReason: nil, dispatchWaitSince: nil
        )
        func status(_ id: String, runtime: WorkTaskRuntime?, nextUp: Bool) -> String? {
            WorkCardLiveStatus.resolve(
                task: card(id), column: .doing, runtime: runtime, liveState: nil, isNextUp: nextUp
            )
        }
        XCTAssertEqual(status("next", runtime: runtime, nextUp: true), "Next up — worker pool full")
        XCTAssertEqual(status("next", runtime: plain, nextUp: true), "Next up")
        XCTAssertEqual(status("later", runtime: plain, nextUp: false), "Queued — reason unknown")
        XCTAssertEqual(status("next", runtime: runtime, nextUp: false), "Queued — worker pool full")
    }

    func testNextUpMarkerReachesTheCardSnapshotOnlyFromEngineDispatch() {
        let model = ChatViewModel(socketPath: "/tmp/queued-nextup-\(UUID().uuidString).sock")
        let next = card("next"), other = card("other")
        model.choresByProductID = ["product": [next, other]]
        model.taskRuntimesByID = [
            "next": WorkTaskRuntime(
                workItemID: "next", executionStatus: "ready", runStatus: nil, executionID: "e1",
                dispatchRetryAt: nil, dispatchWaitReason: nil, dispatchWaitSince: nil
            ),
            "other": WorkTaskRuntime(
                workItemID: "other", executionStatus: "ready", runStatus: nil, executionID: "e2",
                dispatchRetryAt: nil, dispatchWaitReason: nil, dispatchWaitSince: nil
            ),
        ]
        model.applyEventForTest(.workerLiveStatesList(states: [], queuedDispatch: [
            "next": QueuedDispatch(pool: .main, position: 0, nextUp: true),
            "other": QueuedDispatch(pool: .review, position: 1),
        ]))

        func liveStatus(_ task: WorkTask) -> String? {
            model.workCardSnapshot(
                for: task, column: .doing, isSelected: false, isFrontierHighlighted: false,
                boardStyle: .classic, liveState: nil
            ).liveStatus
        }
        XCTAssertEqual(liveStatus(next), "Next up")
        XCTAssertEqual(liveStatus(other), "Queued — reason unknown")
    }
}
