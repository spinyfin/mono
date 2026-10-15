import XCTest
@testable import Boss

/// Tests for optimistic kanban drag-drop: the card moves immediately on drop
/// and either reconciles when the engine confirms or bounces back on failure.
@MainActor
final class OptimisticKanbanMoveTests: XCTestCase {

    // MARK: - Optimistic column override on drop

    func testDropMovesCardImmediately() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        // Card starts in Backlog.
        XCTAssertEqual(model.workItems(in: .backlog).map(\.id), [task.id])
        XCTAssertTrue(model.workItems(in: .doing).isEmpty)

        // Drop into Doing — attemptDrop should return true and the card
        // should appear in Doing before any engine response.
        let accepted = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        XCTAssertTrue(accepted, "drop should be accepted")
        XCTAssertTrue(model.workItems(in: .doing).map(\.id).contains(task.id),
                      "card must appear in Doing immediately after drop")
        XCTAssertFalse(model.workItems(in: .backlog).map(\.id).contains(task.id),
                       "card must leave Backlog immediately after drop")
    }

    func testOptimisticOverrideRespectedWhileInFlight() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)

        // Engine has not yet responded; tasksByProjectID still has old status.
        // The card must stay in Doing via the optimistic override.
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)
    }

    // MARK: - Reconciliation after engine confirmation

    func testWorkItemUpdatedAppliesIncrementalUpdateForChore() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)

        // Engine confirms the move via work_item_updated alone — no full tree fetch.
        let confirmedTask = makeTask(status: "active", id: task.id)
        model.applyEventForTest(.workItemUpdated(item: .chore(confirmedTask)))

        // Override is cleared and the in-memory store is updated, without a work tree.
        XCTAssertEqual(model.effectiveBoardColumn(for: confirmedTask), .doing,
                       "card must be in Doing after work_item_updated without a tree refresh")
        XCTAssertTrue(model.workItems(in: .doing).map(\.id).contains(task.id),
                      "card must appear in Doing after incremental update")
        XCTAssertFalse(model.workItems(in: .backlog).map(\.id).contains(task.id),
                       "card must not remain in Backlog after incremental update")
    }

    func testWorkItemUpdatedAppliesIncrementalUpdateForProjectTask() {
        let model = makeModel()
        let project = makeProject()
        model.projectsByProductID = ["prod_test": [project]]
        let task = makeProjectTask(status: "todo", projectID: project.id)
        model.tasksByProjectID = [project.id: [task]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)

        let confirmedTask = makeProjectTask(status: "active", id: task.id, projectID: project.id)
        model.applyEventForTest(.workItemUpdated(item: .task(confirmedTask)))

        XCTAssertEqual(model.effectiveBoardColumn(for: confirmedTask), .doing,
                       "project task must be in Doing after work_item_updated")
        XCTAssertTrue(model.workItems(in: .doing).map(\.id).contains(task.id),
                      "project task must appear in Doing after incremental update")
    }

    func testReconcileDropsOverrideWhenTreeConfirms() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)

        // Simulate engine sending back a work_item_updated with the new status,
        // then a work tree that reflects the updated status.
        let confirmedTask = makeTask(status: "active", id: task.id)
        model.applyEventForTest(.workItemUpdated(item: .chore(confirmedTask)))
        model.applyEventForTest(
            .workTree(
                product: makeProduct(),
                projects: [],
                tasks: [],
                chores: [confirmedTask],
                taskRuntimes: [],
                dependencies: [],
                ideas: []
            )
        )

        // Override is cleared; real boardColumn (.doing for "active") takes over.
        XCTAssertEqual(model.effectiveBoardColumn(for: confirmedTask), .doing)
        XCTAssertTrue(model.workItems(in: .doing).map(\.id).contains(task.id),
                      "card must still be in Doing after tree reconcile")
    }

    // MARK: - Bounce-back on work_error

    func testWorkErrorBouncesCardBack() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        // Card is now optimistically in Doing.
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)

        // Simulate engine rejecting the move.
        model.applyEventForTest(.workError(message: "transition not allowed", requestId: nil))

        // Override cleared; card returns to its real boardColumn (.backlog for "todo").
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog)
        XCTAssertTrue(model.workItems(in: .backlog).map(\.id).contains(task.id),
                      "card must bounce back to Backlog after work_error")
        XCTAssertFalse(model.workItems(in: .doing).map(\.id).contains(task.id),
                       "card must not remain in Doing after work_error")
    }

    func testWorkErrorShowsInlineNoticeNotModal() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        model.applyEventForTest(.workError(message: "test error", requestId: nil))

        // Inline drag-refusal notice is set for the affected task.
        XCTAssertNotNil(model.dragRefusalNotice, "inline notice must be shown")
        XCTAssertEqual(model.dragRefusalNotice?.taskID, task.id)
        // Modal (workErrorMessage) must NOT be set for a kanban-move error.
        XCTAssertNil(model.workErrorMessage,
                     "modal must not interrupt when an optimistic move is in-flight")
    }

    /// The engine's structured refusal bounces the card and shows the engine's
    /// reason, code and suggested next step on the dragged card.
    func testWorkItemRefusedShowsEngineReasonAndActionOnCard() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)

        let reason = "This item already has a PR (#3163). To change it, add a revision."
        model.applyEventForTest(.workItemRefused(
            id: task.id, code: "bound_pr", message: reason, suggestedAction: "add_revision"
        ))

        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog, "a refused drop bounces back")
        XCTAssertEqual(model.dragRefusalNotice?.taskID, task.id)
        XCTAssertEqual(model.dragRefusalNotice?.message, reason)
        XCTAssertEqual(model.dragRefusalNotice?.code, "bound_pr")
        XCTAssertEqual(model.dragRefusalNotice?.suggestedAction, "add_revision")
        XCTAssertNil(model.workErrorMessage, "a board refusal is inline, not modal")

        model.performDragRefusalAction(model.dragRefusalNotice!)
        XCTAssertEqual(model.revisionComposerTaskID, task.id, "the action opens the revision composer for the refused card")
        XCTAssertNil(model.dragRefusalNotice)
    }

    /// Submitting the composer files a revision against the refused card.
    func testSubmitRevisionSendsCreateRevisionForParent() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]
        var sent: [[String: Any]] = []
        model.outboundRecorder = { sent.append($0) }
        model.revisionComposerTaskID = task.id

        model.submitRevision(parentTaskID: task.id, description: "   ")
        XCTAssertTrue(sent.isEmpty, "blank text must not file a revision")
        XCTAssertEqual(model.revisionComposerTaskID, task.id)

        model.submitRevision(parentTaskID: task.id, description: "  tighten the error text \n")
        XCTAssertEqual(sent.count, 1)
        XCTAssertEqual(sent.first?["type"] as? String, "create_revision")
        XCTAssertEqual(sent.first?["parent_task_id"] as? String, task.id)
        XCTAssertEqual(sent.first?["description"] as? String, "tighten the error text")
        XCTAssertNil(model.revisionComposerTaskID)
    }

    /// A refusal for one card must not roll back another card's in-flight move.
    func testWorkItemRefusedBouncesOnlyTheRefusedCard() {
        let model = makeModel()
        let a = makeTask(status: "todo")
        let b = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [a, b]]
        _ = model.attemptDrop(a.id, onColumn: .doing, group: nil)
        _ = model.attemptDrop(b.id, onColumn: .doing, group: nil)

        model.applyEventForTest(.workItemRefused(
            id: a.id, code: "update_rejected", message: "nope", suggestedAction: nil
        ))

        XCTAssertEqual(model.effectiveBoardColumn(for: a), .backlog, "refused card bounces")
        XCTAssertEqual(model.effectiveBoardColumn(for: b), .doing, "other card's move stays in flight")
        XCTAssertNotNil(model.pendingMoveOriginByTaskID[b.id])
        XCTAssertNil(model.pendingMoveOriginByTaskID[a.id])
        XCTAssertEqual(model.dragRefusalNotice?.taskID, a.id)
    }

    /// A refusal for a card with no pending move leaves every pending move alone.
    func testWorkItemRefusedForNonPendingCardKeepsPendingMoves() {
        let model = makeModel()
        let moving = makeTask(status: "todo")
        let idle = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [moving, idle]]
        _ = model.attemptDrop(moving.id, onColumn: .doing, group: nil)

        model.applyEventForTest(.workItemRefused(
            id: idle.id, code: "bound_pr", message: "already has a PR", suggestedAction: nil
        ))

        XCTAssertEqual(model.effectiveBoardColumn(for: moving), .doing)
        XCTAssertNotNil(model.pendingMoveOriginByTaskID[moving.id])
        XCTAssertEqual(model.dragRefusalNotice?.taskID, idle.id)
    }

    /// The popover's Move buttons go through the board-drop RPC, so the engine
    /// can answer with a structured refusal.
    func testMoveTaskSendsMoveWorkItemOnBoard() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]
        var sent: [[String: Any]] = []
        model.outboundRecorder = { sent.append($0) }

        model.moveTask(task.id, to: .done)

        XCTAssertEqual(sent.count, 1)
        XCTAssertEqual(sent.first?["type"] as? String, "move_work_item_on_board")
        XCTAssertEqual(sent.first?["id"] as? String, task.id)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .done)

        model.applyEventForTest(.workItemRefused(
            id: task.id, code: "bound_pr", message: "already has a PR", suggestedAction: "add_revision"
        ))
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog)
        XCTAssertEqual(model.dragRefusalNotice?.code, "bound_pr")
    }

    /// A refused Move Up/Down reorder lands on the moved card, not a modal.
    func testReorderWorkErrorShowsOnMovedCard() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]
        model.reorderRequestTaskIDs["req-1"] = task.id

        model.applyEventForTest(.workError(message: "cannot reorder", requestId: "req-1"))

        XCTAssertEqual(model.dragRefusalNotice?.taskID, task.id)
        XCTAssertEqual(model.dragRefusalNotice?.message, "cannot reorder")
        XCTAssertNil(model.workErrorMessage)
        XCTAssertTrue(model.reorderRequestTaskIDs.isEmpty)
    }

    /// A refusal still surfaces on the card when no optimistic move is pending.
    func testWorkItemRefusedWithoutPendingMoveStillShowsNotice() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        model.applyEventForTest(.workItemRefused(
            id: task.id, code: "update_rejected", message: "cannot move: gated by [task_p2]", suggestedAction: nil
        ))

        XCTAssertEqual(model.dragRefusalNotice?.taskID, task.id)
        XCTAssertEqual(model.dragRefusalNotice?.message, "cannot move: gated by [task_p2]")
        XCTAssertNil(model.dragRefusalNotice?.suggestedAction)
    }

    func testWorkErrorFallsBackToModalWhenNoMovePending() {
        let model = makeModel()
        // No pending optimistic move.
        model.applyEventForTest(.workError(message: "some other error", requestId: nil))
        XCTAssertNotNil(model.workErrorMessage,
                        "modal must still show when no move is in-flight")
    }

    // MARK: - No flicker: unrelated engine pushes don't clobber optimistic position

    func testUnrelatedWorkTreeDoesNotClobberOptimisticCard() {
        let model = makeModel()
        let task = makeTask(status: "todo", id: "task_a")
        let other = makeTask(status: "active", id: "task_b")
        model.choresByProductID = ["prod_test": [task, other]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)

        // Engine sends a tree update (unrelated, task_a still has old status).
        model.applyEventForTest(
            .workTree(
                product: makeProduct(),
                projects: [],
                tasks: [],
                chores: [task, other],  // task_a still "todo"
                taskRuntimes: [],
                dependencies: [],
                ideas: []
            )
        )

        // Optimistic override must still hold — real state is "todo" but move
        // is still pending confirmation.
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing,
                       "optimistic position must survive an unrelated tree push")
    }

    // MARK: - Dispatch-pending autostart case

    func testDispatchPendingDropToBacklogIsOptimistic() {
        let model = makeModel()
        let task = makeTask(status: "todo", autostart: true)
        model.choresByProductID = ["prod_test": [task]]

        // Dispatch-pending task shows in Doing.
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)

        // Drop to Backlog (cancels dispatch).
        let accepted = model.attemptDrop(task.id, onColumn: .backlog, group: nil)
        XCTAssertTrue(accepted)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog,
                       "card must move to Backlog immediately")
    }

    // MARK: - Same-column drops are a client-side no-op

    /// A same-column, same-group drop is a no-op: the client must not stage
    /// an optimistic move and must not send `move_work_item_on_board` at
    /// all. Kanban lanes are not user-ordered, so a drag that ends where it
    /// began carries no user intent to record.
    func testReorderWithinMergingGroupSendsNoRPC() {
        let model = makeModel()
        var task = makeTask(status: "in_review")
        task.mergeQueueState = "queued"
        model.choresByProductID = ["prod_test": [task]]
        var sent: [[String: Any]] = []
        model.outboundRecorder = { sent.append($0) }

        XCTAssertEqual(model.effectiveBoardColumn(for: task), .done)
        XCTAssertEqual(model.boardGroup(for: task), .merging)

        let accepted = model.attemptDrop(task.id, onColumn: .done, group: .merging)
        XCTAssertTrue(accepted, "a same-column drop is a handled no-op, not a refusal")
        XCTAssertNil(model.optimisticColumnByTaskID[task.id],
                     "a same-column drop must not stage an optimistic column change")
        XCTAssertNil(model.pendingMoveOriginByTaskID[task.id],
                     "a same-column drop has no origin to bounce back to")
        XCTAssertEqual(model.boardGroup(for: task), .merging,
                       "card must stay in Merging")
        XCTAssertTrue(sent.isEmpty, "a same-column drop must not send any RPC to the engine")
    }

    /// A drop that misses every group and lands on the Done column's padding
    /// is unqualified — less intent than a group-qualified drop, not more. It
    /// must not stage a move, and must not send an RPC, for a card already
    /// in that column.
    func testUnqualifiedDropOnOwnColumnSendsNoRPC() {
        let model = makeModel()
        var task = makeTask(status: "in_review")
        task.mergeQueueState = "queued"
        model.choresByProductID = ["prod_test": [task]]
        var sent: [[String: Any]] = []
        model.outboundRecorder = { sent.append($0) }

        XCTAssertTrue(model.attemptDrop(task.id, onColumn: .done, group: nil))
        XCTAssertNil(model.optimisticColumnByTaskID[task.id])
        XCTAssertNil(model.pendingMoveOriginByTaskID[task.id])
        XCTAssertTrue(sent.isEmpty, "a same-column drop must not send any RPC to the engine")
    }

    /// The legitimate transition the fix must preserve: dragging an in-flight
    /// merge out of Merging and into a completion group means "this is
    /// done", and it is distinguishable from the same-column drop above
    /// purely by the group — it is a real change and must still be sent.
    func testMergingToCompletionGroupStillSendsRPC() {
        let model = makeModel()
        var task = makeTask(status: "in_review")
        task.mergeQueueState = "queued"
        model.choresByProductID = ["prod_test": [task]]
        var sent: [[String: Any]] = []
        model.outboundRecorder = { sent.append($0) }

        XCTAssertTrue(model.attemptDrop(task.id, onColumn: .done, group: .completed))
        XCTAssertEqual(model.optimisticColumnByTaskID[task.id], .done,
                       "a cross-group drop is a real move and is staged optimistically")
        XCTAssertEqual(model.pendingMoveOriginByTaskID[task.id], .done)
        XCTAssertEqual(sent.count, 1, "a cross-group drop is a real change and must be sent")
        XCTAssertEqual(sent.first?["type"] as? String, "move_work_item_on_board")
    }

    /// Same rule in a column with no groups: dropping a dispatch-pending
    /// card back into Doing must not force it to start, and must not send
    /// an RPC.
    func testReorderWithinDoingSendsNoRPC() {
        let model = makeModel()
        let task = makeTask(status: "todo", autostart: true)
        model.choresByProductID = ["prod_test": [task]]
        var sent: [[String: Any]] = []
        model.outboundRecorder = { sent.append($0) }

        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)
        XCTAssertTrue(model.attemptDrop(task.id, onColumn: .doing, group: nil))
        XCTAssertNil(model.optimisticColumnByTaskID[task.id])
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)
        XCTAssertTrue(sent.isEmpty, "a same-column drop must not send any RPC to the engine")
    }

    /// Cross-column drags are unaffected: they still send
    /// `move_work_item_on_board` and still move the card. Uses a
    /// dispatch-pending card dropped to Backlog (rather than into Doing) so
    /// the drop takes the direct `sendMoveWorkItemOnBoard` path rather than
    /// the dispatch-admission detour that a drop *into* Doing triggers.
    func testCrossColumnDropStillSendsRPCAndMoves() {
        let model = makeModel()
        let task = makeTask(status: "todo", autostart: true)
        model.choresByProductID = ["prod_test": [task]]
        var sent: [[String: Any]] = []
        model.outboundRecorder = { sent.append($0) }

        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)
        let accepted = model.attemptDrop(task.id, onColumn: .backlog, group: nil)
        XCTAssertTrue(accepted)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog,
                       "card must move to Backlog immediately after a cross-column drop")
        XCTAssertEqual(sent.count, 1, "a cross-column drop is a real change and must be sent")
        XCTAssertEqual(sent.first?["type"] as? String, "move_work_item_on_board")
    }

    func testDragWhileDisconnectedPreservesRetryAndDiscardBouncesBack() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]
        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)

        model.applyEventForTest(.notConnected(requestKind: "evaluate_dispatch_admission"))

        XCTAssertNil(model.workErrorMessage, "disconnect must not raise the modal")
        XCTAssertNotNil(model.pendingDragAdmissionCheck, "Retry must retain the continuation")
        XCTAssertEqual(model.engine.unsentActionCountForTesting, 1)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)
        model.dismissDisconnectedActions()
        XCTAssertNil(model.pendingDragAdmissionCheck)
        XCTAssertFalse(model.engine.hasUnsentActions)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog)
    }

    func testTwoDisconnectedDragsKeepIndependentContinuations() {
        let model = makeModel()
        let first = makeTask(status: "todo")
        let second = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [first, second]]
        _ = model.attemptDrop(first.id, onColumn: .doing, group: nil)
        _ = model.attemptDrop(second.id, onColumn: .doing, group: nil)
        XCTAssertEqual(Set(model.pendingDragAdmissionChecks.keys), [first.id, second.id])

        model.engine.simulateConnectedForTesting = true
        var sent: [[String: Any]] = []
        model.outboundRecorder = { sent.append($0) }
        for task in [first, second] {
            model.handleDispatchAdmissionEvaluated(DispatchAdmission(
                workItemID: task.id,
                wouldDispatch: true,
                pause: DispatchPauseSnapshot(active: false, origin: nil, reason: nil, pausedSinceEpochS: nil, overridable: false),
                blockers: []
            ))
        }
        let moved = sent.filter { $0["type"] as? String == "move_work_item_on_board" }.compactMap { $0["id"] as? String }
        XCTAssertEqual(Set(moved), [first.id, second.id])
        XCTAssertEqual(moved.count, 2, "exactly one move per task")
        XCTAssertTrue(model.pendingDragAdmissionChecks.isEmpty)
    }

    func testUnrelatedDroppedRequestsPreservePendingDrag() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]
        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        for kind in ["get_settings", "create_chore", "update_work_item"] {
            model.applyEventForTest(.notConnected(requestKind: kind))
            XCTAssertNotNil(model.pendingDragAdmissionCheck)
            XCTAssertNotNil(model.pendingMoveOriginByTaskID[task.id])
            XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)
            XCTAssertNil(model.dragRefusalNotice)
        }
    }

    func testDiscardingUnrelatedQueuedActionPreservesLiveDrag() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]
        model.engine.simulateConnectedForTesting = true
        XCTAssertTrue(model.attemptDrop(task.id, onColumn: .doing, group: nil))
        model.engine.simulateConnectedForTesting = false
        model.engine.sendLine(["type": "create_product", "name": "Queued product"])

        model.dismissDisconnectedActions()

        XCTAssertNotNil(model.pendingDragAdmissionCheck)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)
        XCTAssertFalse(model.engine.hasUnsentActions)
    }

    // MARK: - Helpers

    private func makeTask(
        status: String,
        autostart: Bool = false,
        id: String? = nil
    ) -> WorkTask {
        WorkTask(
            id: id ?? "task_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Test",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-06-01T00:00:00Z",
            updatedAt: "2026-06-01T00:00:00Z",
            autostart: autostart
        )
    }

    private func makeProjectTask(
        status: String,
        id: String? = nil,
        projectID: String
    ) -> WorkTask {
        WorkTask(
            id: id ?? "task_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: projectID,
            kind: "task",
            name: "Test Project Task",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-06-01T00:00:00Z",
            updatedAt: "2026-06-01T00:00:00Z"
        )
    }

    private func makeProject() -> WorkProject {
        WorkProject(
            id: "proj_test",
            productID: "prod_test",
            name: "Test Project",
            slug: "test",
            description: "",
            goal: "",
            status: "active",
            priority: "medium",
            createdAt: "2026-06-01T00:00:00Z",
            updatedAt: "2026-06-01T00:00:00Z"
        )
    }

    private func makeProduct() -> WorkProduct {
        WorkProduct(
            id: "prod_test",
            name: "Test Product",
            slug: "test",
            description: "",
            repoRemoteURL: nil,
            status: "active",
            createdAt: "2026-06-01T00:00:00Z",
            updatedAt: "2026-06-01T00:00:00Z"
        )
    }

    private func makeModel() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.products = [makeProduct()]
        model.selectedWorkProductID = "prod_test"
        return model
    }
}
