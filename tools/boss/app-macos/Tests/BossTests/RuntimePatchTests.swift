import XCTest
@testable import Boss

/// The app's half of the runtime/structural invalidation split (hub-and-spoke
/// design §2.4). A `work_invalidated` push classed `runtime` makes a burst
/// re-read only the cards it names (`get_work_runtime_patch`); a burst that
/// holds anything else re-reads the whole board exactly as before. A patch is
/// applied only when it changes runtime fields: a move, a status change, a
/// deletion or a card the app does not hold falls back to the board, so a
/// misclassified push costs a full read rather than a stale board.
/// See `ChatViewModel+RuntimePatch.swift`.
@MainActor
final class RuntimePatchTests: XCTestCase {

    // MARK: - Wire

    func testWorkInvalidatedDecodesItsClassAndDefaultsToStructural() async {
        let (model, recorder) = makeModel()

        model.engine.consumeLineForTesting(invalidationLine(class: "runtime", itemIDs: ["task_1"]))
        await waitFor(recorder, "get_work_runtime_patch")
        XCTAssertEqual(recorder.counts["get_work_tree"], nil, "a runtime push is not a whole-board read")
        recorder.reset()

        model.engine.consumeLineForTesting(invalidationLine(class: nil, itemIDs: ["task_1"]))
        await waitFor(recorder, "get_work_tree")
        XCTAssertEqual(
            recorder.counts["get_work_runtime_patch"], nil,
            "an engine that sends no class is read as structural"
        )
        recorder.reset()

        model.engine.consumeLineForTesting(invalidationLine(class: "from_the_future", itemIDs: ["task_1"]))
        await waitFor(recorder, "get_work_tree")
        XCTAssertEqual(recorder.counts["get_work_runtime_patch"], nil, "an unknown class is read as structural")
    }

    func testWorkRuntimePatchReplyDecodesCardsRuntimesAndAbsentIDs() async {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1"), makeTask(id: "task_2")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        recorder.reset()

        model.engine.consumeLineForTesting(
            """
            {"revision":19,"request_id":"\(model.boardReadFences["prod_a"]!.patchRequestID!)","payload":{"type":"work_runtime_patch","product_id":"prod_a","absent":["task_gone"],"items":[\
            {"item":{"item_type":"task","id":"task_1","product_id":"prod_a","project_id":"proj_a1","kind":"project_task",\
            "name":"Task task_1","description":"","status":"todo","priority":"medium","created_at":"2026-07-10T00:00:00Z",\
            "updated_at":"2026-07-10T00:00:00Z","ci_required_state":"success"},\
            "runtime":{"work_item_id":"task_1","execution_status":"ready","dispatch_wait_reason":"pool_exhausted"}}]}}
            """
        )
        await waitUntil { model.task(withID: "task_1")?.ciRequiredState == "success" }

        XCTAssertEqual(model.taskRuntime(for: "task_1")?.dispatchWaitReason, "pool_exhausted")
        XCTAssertEqual(model.taskRuntime(for: "task_1")?.executionStatus, "ready")
        XCTAssertEqual(model.boardReadFences["prod_a"]?.revision, 19)
        XCTAssertTrue(recorder.sent.isEmpty, "a patch that changes only runtime fields needs no further read")
    }

    func testMalformedCardMakesThePatchIncompleteSoItIsNotTrusted() async {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        recorder.reset()

        model.engine.consumeLineForTesting(
            """
            {"request_id":"\(model.boardReadFences["prod_a"]!.patchRequestID!)","payload":{"type":"work_runtime_patch","product_id":"prod_a","items":[{"item":{"item_type":"task","id":"task_1"}}]}}
            """
        )
        await waitFor(recorder, "get_work_tree")
        XCTAssertEqual(recorder.counts["get_work_tree"], 1, "an undecodable card falls back to the board")
    }

    // MARK: - Routing

    func testRuntimeOnlyBurstSendsOnePatchOfTheUnionAndNoTree() async {
        let (model, recorder) = makeModel(cards: (1...10).map { makeTask(id: "task_\($0)") })

        for index in 1...10 {
            model.applyEventForTest(runtimeInvalidation(["task_\(index)"]))
        }
        XCTAssertTrue(recorder.sent.isEmpty, "nothing is sent while the window is open; got \(recorder.counts)")
        await waitFor(recorder, "get_work_runtime_patch")

        XCTAssertEqual(
            recorder.counts,
            ["get_work_runtime_patch": 1, "list_engine_attempts": 1],
            "ten runtime pushes cost one patch: no tree and no deferred-scope list"
        )
        XCTAssertEqual(recorder.patchItemIDs, (1...10).map { "task_\($0)" }.sorted())
    }

    func testOneStructuralPushMakesTheWholeBurstATreeRead() async {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1"), makeTask(id: "task_2")])

        model.applyEventForTest(runtimeInvalidation(["task_1"]))
        model.applyEventForTest(.workInvalidated(
            topic: "work.product.prod_a", productId: "prod_a", itemIds: ["task_2"], invalidationClass: .structural
        ))
        model.applyEventForTest(runtimeInvalidation(["task_1"]))
        await waitFor(recorder, "get_work_tree")

        XCTAssertEqual(recorder.counts["get_work_tree"], 1)
        XCTAssertEqual(recorder.counts["list_deferred_scope_attentions"], 1)
        XCTAssertNil(recorder.counts["get_work_runtime_patch"], "a runtime push can never hide a structural one")
    }

    func testRuntimePushNamingNoCardsIsATreeRead() async {
        let (model, recorder) = makeModel()

        model.applyEventForTest(runtimeInvalidation([]))
        await waitFor(recorder, "get_work_tree")

        XCTAssertNil(recorder.counts["get_work_runtime_patch"], "there is nothing to patch")
    }

    func testDroppedInvalidationDuringRuntimePushesIsATreeRead() async {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])

        model.applyEventForTest(runtimeInvalidation(["task_1"]))
        model.applyEventForTest(.resyncRequired)
        await waitFor(recorder, "get_work_tree")

        XCTAssertEqual(recorder.counts["get_work_tree"], 1)
        XCTAssertNil(recorder.counts["get_work_runtime_patch"], "a dropped push could have been structural")
    }

    func testRuntimePushForAnotherProductSendsNoPatchAndNoTree() async {
        let (model, recorder) = makeModel()

        model.applyEventForTest(.workInvalidated(
            topic: model.workTopic(forProductID: "prod_b"), productId: "prod_b", itemIds: ["task_b"],
            invalidationClass: .runtime
        ))
        await waitFor(recorder, "list_engine_attempts")

        XCTAssertNil(recorder.counts["get_work_runtime_patch"])
        XCTAssertNil(recorder.counts["get_work_tree"])
    }

    func testBurstNamingMoreCardsThanAPatchIsWorthIsATreeRead() async {
        let count = ChatViewModel.maxRuntimePatchItems + 1
        let (model, recorder) = makeModel()

        model.applyEventForTest(runtimeInvalidation((0..<count).map { "task_\($0)" }))
        await waitFor(recorder, "get_work_tree")

        XCTAssertNil(recorder.counts["get_work_runtime_patch"], "\(count) cards: the tree is the cheaper read")
    }

    func testRuntimePushWhileATreeIsOutstandingWaitsForItAndThenSendsOnePatch() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1"), makeTask(id: "task_2")])

        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        recorder.reset()
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_2"])

        XCTAssertTrue(recorder.sent.isEmpty, "an older tree could land after the patch and undo it; a second tree is dearer than waiting")

        model.applyEventForTest(makeWorkTreeEvent(
            tasks: [makeTask(id: "task_1"), makeTask(id: "task_2")],
            requestID: model.boardReadFences["prod_a"]?.trees.keys.first
        ))

        XCTAssertEqual(recorder.patchBodies, [["task_1", "task_2"]])
        XCTAssertNil(recorder.counts["get_work_tree"])
    }

    func testAnAnsweredTreeNoLongerBlocksPatches() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        model.applyEventForTest(makeWorkTreeEvent(tasks: [makeTask(id: "task_1")], requestID: model.boardReadFences["prod_a"]?.trees.keys.first))
        recorder.reset()

        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])

        XCTAssertEqual(recorder.counts, ["get_work_runtime_patch": 1])
    }

    func testCardsThatAskWhileAPatchIsInFlightGoOutTogetherWhenItsReplyLands() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1"), makeTask(id: "task_2"), makeTask(id: "task_3")])

        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_2"])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_3"])
        XCTAssertEqual(recorder.patchBodies.count, 1, "one patch in flight per product")

        model.applyEventForTest(patchReply(model: model, cards: [(makeTask(id: "task_1"), runtime("task_1"))]))

        XCTAssertEqual(recorder.patchBodies.count, 2)
        XCTAssertEqual(recorder.patchBodies.last, ["task_2", "task_3"])
        XCTAssertNil(recorder.counts["get_work_tree"])
    }

    func testATreeRequestedAfterAPatchSupersedesItsReply() {
        var stale = makeTask(id: "task_1")
        stale.ciRequiredState = "fail"
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])

        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        recorder.reset()
        model.applyEventForTest(patchReply(model: model, cards: [(stale, runtime("task_1"))]))

        XCTAssertNil(
            model.task(withID: "task_1")?.ciRequiredState,
            "the patch was read before the tree that follows it; applying it could undo newer state"
        )
        XCTAssertTrue(recorder.sent.isEmpty, "the tree already on its way covers the card")
    }

    func testAnErrorWhileAPatchIsInFlightRefetchesTheBoard() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        recorder.reset()

        model.applyEventForTest(.workError(message: "database busy", requestId: model.boardReadFences["prod_a"]?.patchRequestID))

        XCTAssertEqual(recorder.counts["get_work_tree"], 1, "a matching error must re-read the card")
    }

    func testAReplyNobodyIsWaitingForIsIgnored() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])

        model.applyEventForTest(patchReply(model: model, cards: [(makeTask(id: "task_1"), runtime("task_1"))]))

        XCTAssertTrue(recorder.sent.isEmpty, "a retired reply must not trigger another read")
    }

    func testADisconnectForgetsOutstandingReads() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        model.applyEventForTest(.disconnected)
        model.isConnected = true
        recorder.reset()

        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])

        XCTAssertEqual(recorder.counts, ["get_work_runtime_patch": 1], "the tree died with the connection")
    }

    func testSupersededReplyDrainsInvalidationsAfterTreeApplied() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        let reply = patchReply(model: model, cards: [])
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        model.applyEventForTest(makeWorkTreeEvent(
            tasks: [makeTask(id: "task_1")],
            requestID: model.boardReadFences["prod_a"]?.trees.keys.first
        ))
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        recorder.reset()
        model.applyEventForTest(reply)
        XCTAssertEqual(recorder.patchBodies, [["task_1"]])
    }

    func testFailedTreeThenSuccessfulTreeAllowsPatch() {
        let (model, recorder) = makeModel()
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        let failedID = model.boardReadFences["prod_a"]!.trees.keys.first!
        model.applyEventForTest(.workError(message: "read failed", requestId: failedID))
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        model.applyEventForTest(makeWorkTreeEvent(
            tasks: [], requestID: model.boardReadFences["prod_a"]?.trees.keys.first
        ))
        recorder.reset()
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        XCTAssertEqual(recorder.counts, ["get_work_runtime_patch": 1])
    }

    func testUndecodableTreeSettlesItsRequest() async {
        let (model, recorder) = makeModel()
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        let id = model.boardReadFences["prod_a"]!.trees.keys.first!
        model.engine.consumeLineForTesting(
            "{\"request_id\":\"\(id)\",\"payload\":{\"type\":\"work_tree\",\"product\":{}}}"
        )
        await waitUntil { model.boardReadFences["prod_a"]!.trees.isEmpty }
        XCTAssertTrue(model.boardReadFences["prod_a"]!.trees.isEmpty)
        recorder.reset()
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        XCTAssertEqual(recorder.counts, ["get_work_tree": 1, "list_deferred_scope_attentions": 1])
        XCTAssertTrue(model.boardReadFences["prod_a"]!.needsTree, "decode failure still owes a structural refresh")
    }

    func testUnrelatedErrorLeavesPatchOutstanding() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        let id = model.boardReadFences["prod_a"]?.patchRequestID
        recorder.reset()
        model.applyEventForTest(.workError(message: "mutation rejected", requestId: "another-request"))
        XCTAssertEqual(model.boardReadFences["prod_a"]?.patchRequestID, id)
        XCTAssertTrue(recorder.sent.isEmpty)
        var updated = makeTask(id: "task_1")
        updated.ciRequiredState = "success"
        model.applyEventForTest(patchReply(model: model, cards: [(updated, runtime("task_1"))]))
        XCTAssertEqual(model.task(withID: "task_1")?.ciRequiredState, "success")
    }

    func testLateExpiredTreeCannotOverwriteNewerPatch() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        let treeID = model.boardReadFences["prod_a"]!.trees.keys.first!
        model.boardReadFences["prod_a"]!.trees[treeID] = .now - .seconds(31)
        model.boardReadFences["prod_a"]!.expire(isQueued: { _ in false })
        var updated = makeTask(id: "task_1")
        updated.ciRequiredState = "success"
        model.applyEventForTest(.workItemUpdated(item: .task(updated), revision: 12))
        recorder.reset()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [makeTask(id: "task_1")], requestID: treeID, revision: 11))
        XCTAssertEqual(model.task(withID: "task_1")?.ciRequiredState, "success")
        XCTAssertEqual(recorder.counts["get_work_tree"], 1)
    }

    func testExpiredTreeRetainsDeletionObligation() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "deleted"), makeTask(id: "updated")])
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        let treeID = model.boardReadFences["prod_a"]!.trees.keys.first!
        model.boardReadFences["prod_a"]!.trees[treeID] = .now - .seconds(31)
        recorder.reset()
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["updated"])
        XCTAssertEqual(recorder.counts["get_work_tree"], 1)
        XCTAssertTrue(recorder.patchBodies.isEmpty)
        XCTAssertTrue(model.boardReadFences["prod_a"]!.needsTree)
        let replacementID = model.boardReadFences["prod_a"]!.trees.keys.first!
        model.applyEventForTest(makeWorkTreeEvent(tasks: [makeTask(id: "updated")], requestID: replacementID, revision: 12))
        XCTAssertNil(model.task(withID: "deleted"))
        XCTAssertFalse(model.boardReadFences["prod_a"]!.needsTree)
    }

    func testQueuedTreeReregistersAfterSecondDisconnect() {
        let (model, recorder) = makeModel()
        model.engine.simulateConnectedForTesting = false
        model.applyEventForTest(.disconnected)
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        let treeID = model.boardReadFences["prod_a"]!.trees.keys.first!
        model.applyEventForTest(.disconnected)
        XCTAssertNotNil(model.boardReadFences["prod_a"]?.trees[treeID])
        model.boardReadFences["prod_a"]!.trees[treeID] = .now - .seconds(31)
        model.boardReadFences["prod_a"]!.expire(isQueued: model.engine.isReadQueued(requestId:))
        XCTAssertNotNil(model.boardReadFences["prod_a"]?.trees[treeID])
        model.engine.simulateConnectedForTesting = true
        model.engine.resetConnectTreeClaims()
        model.engine.flushQueuedReads()
        XCTAssertNotNil(model.boardReadFences["prod_a"]?.trees[treeID])
        XCTAssertLessThan(model.boardReadFences["prod_a"]!.trees[treeID]!.duration(to: .now), .seconds(30))
        recorder.reset()
        model.applyEventForTest(.connected)
        XCTAssertNil(recorder.counts["get_work_tree"])
        model.applyEventForTest(makeWorkTreeEvent(tasks: [makeTask(id: "task_1")], requestID: treeID, revision: 12))
        XCTAssertNotNil(model.task(withID: "task_1"))
    }

    func testQueuedTreeReplacementRetiresOldFence() {
        let (model, _) = makeModel()
        model.engine.simulateConnectedForTesting = false
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        let oldID = model.boardReadFences["prod_a"]!.trees.keys.first!
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        XCTAssertNil(model.boardReadFences["prod_a"]!.trees[oldID])
        XCTAssertEqual(model.boardReadFences["prod_a"]!.trees.count, 1)
    }

    func testFailedTreeRetainsDeletionUntilReplacementAccepted() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "deleted"), makeTask(id: "updated")])
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        let failedID = model.boardReadFences["prod_a"]!.trees.keys.first!
        recorder.reset()
        model.applyEventForTest(.workError(message: "read failed", requestId: failedID))
        XCTAssertTrue(recorder.sent.isEmpty, "errors must not start a retry loop")
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["updated"])
        XCTAssertEqual(recorder.counts["get_work_tree"], 1)
        XCTAssertTrue(recorder.patchBodies.isEmpty)
        XCTAssertTrue(model.boardReadFences["prod_a"]!.needsTree)
        let replacementID = model.boardReadFences["prod_a"]!.trees.keys.first!
        model.applyEventForTest(makeWorkTreeEvent(tasks: [makeTask(id: "updated")], requestID: failedID, revision: 12))
        XCTAssertTrue(model.boardReadFences["prod_a"]!.needsTree, "a retired read cannot settle the obligation")
        model.applyEventForTest(makeWorkTreeEvent(tasks: [makeTask(id: "updated")], requestID: replacementID, revision: 12))
        XCTAssertNil(model.task(withID: "deleted"))
        XCTAssertFalse(model.boardReadFences["prod_a"]!.needsTree)
    }

    func testMalformedTreeDeltaDoesNotFailOptimisticMove() async {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.optimisticColumnByTaskID["task_1"] = .doing
        model.engine.sendGetWorkTree(productId: "prod_a", flow: .invalidationRefetch)
        let treeID = model.boardReadFences["prod_a"]!.trees.keys.first!
        recorder.reset()
        model.engine.consumeLineForTesting(
            "{\"request_id\":\"\(treeID)\",\"payload\":{\"type\":\"work_tree_update\",\"update\":{\"product_id\":\"prod_a\",\"base_sequence\":999}}}"
        )
        await waitFor(recorder, "get_work_tree")
        XCTAssertEqual(model.optimisticColumnByTaskID["task_1"], .doing)
        XCTAssertNil(model.workErrorMessage)
        XCTAssertNil(model.boardReadFences["prod_a"]?.trees[treeID])
    }

    func testSlowUnretiredPatchReplyStillDrainsQueuedIDs() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_2"])
        model.boardReadFences["prod_a"]!.patchSentAt = .now - .seconds(31)
        recorder.reset()
        model.applyEventForTest(patchReply(model: model, cards: []))
        XCTAssertEqual(recorder.patchBodies, [["task_2"]])
    }

    func testNewTreeDoesNotExtendOlderTreesExpiry() {
        let (model, recorder) = makeModel()
        model.noteWorkTreeRequested(productID: "prod_a", requestID: "old")
        model.boardReadFences["prod_a"]!.trees["old"] = .now - .seconds(31)
        model.noteWorkTreeRequested(productID: "prod_a", requestID: "new")
        model.applyEventForTest(makeWorkTreeEvent(tasks: [], requestID: "new"))
        recorder.reset()
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        XCTAssertEqual(recorder.patchBodies, [["task_1"]])
        XCTAssertTrue(model.boardReadFences["prod_a"]!.trees.isEmpty)
    }

    func testExpiredPatchQueueAndNewBurstOverLimitRefetchTree() {
        let (model, recorder) = makeModel()
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["first"])
        let queued = Set((0..<64).map { "card_\($0)" })
        model.requestRuntimePatch(productID: "prod_a", itemIDs: queued)
        model.boardReadFences["prod_a"]!.patchSentAt = .now - .seconds(31)
        recorder.reset()
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["another"])
        XCTAssertTrue(recorder.patchBodies.isEmpty)
        XCTAssertFalse(model.boardReadFences["prod_a"]!.trees.isEmpty)
        XCTAssertTrue(model.boardReadFences["prod_a"]!.queuedPatchItemIDs.isEmpty)
    }

    func testExpiredPatchReplyCannotSettleReplacementPatch() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        let oldReply = patchReply(model: model, cards: [])
        model.boardReadFences["prod_a"]!.patchSentAt = .now - .seconds(31)
        recorder.reset()
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_2"])
        XCTAssertEqual(recorder.patchBodies, [["task_1", "task_2"]])
        let replacementID = model.boardReadFences["prod_a"]?.patchRequestID
        recorder.reset()
        model.applyEventForTest(oldReply)
        XCTAssertEqual(model.boardReadFences["prod_a"]?.patchRequestID, replacementID)
        XCTAssertTrue(recorder.sent.isEmpty)
    }

    func testPatchOlderThanDirectUpdateCannotRevertRename() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        let requestID = model.boardReadFences["prod_a"]?.patchRequestID
        var renamed = makeTask(id: "task_1")
        renamed.name = "New name"
        model.applyEventForTest(.workItemUpdated(item: .task(renamed), revision: 12))
        recorder.reset()
        model.applyEventForTest(.workRuntimePatch(
            productId: "prod_a",
            cards: [WorkRuntimePatchCard(item: .task(makeTask(id: "task_1")), runtime: runtime("task_1"))],
            absent: [], isComplete: true, requestId: requestID, revision: 11
        ))
        XCTAssertEqual(model.task(withID: "task_1")?.name, "New name")
        XCTAssertEqual(recorder.counts["get_work_tree"], 1)
    }

    // MARK: - Applying a patch

    func testPatchUpdatesRuntimeFieldsInPlaceWithoutReadingTheBoard() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1"), makeTask(id: "task_2")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        recorder.reset()

        var patched = makeTask(id: "task_1")
        patched.ciRequiredState = "success"
        patched.prStatePolledAt = "2026-10-22T10:00:00Z"
        model.applyEventForTest(patchReply(model: model, cards: [(patched, runtime("task_1", waitReason: "pool_exhausted"))]))

        XCTAssertEqual(model.task(withID: "task_1")?.ciRequiredState, "success")
        XCTAssertEqual(model.taskRuntime(for: "task_1")?.dispatchWaitReason, "pool_exhausted")
        XCTAssertEqual(model.tasksByProjectID["proj_a1"]?.map(\.id).sorted(), ["task_1", "task_2"], "no card was added or lost")
        XCTAssertTrue(recorder.sent.isEmpty, "a runtime-only patch needs no tree")
    }

    func testPatchOfARevisionChainUpdatesTheRelatedCardsTogether() {
        var root = makeTask(id: "root")
        root.status = "in_review"
        var revision = makeTask(id: "rev", projectID: nil, kind: "revision", status: "active")
        revision.parentTaskId = "root"
        let (model, recorder) = makeModel(cards: [root], productLevelRevisions: [revision])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["rev"])
        recorder.reset()

        var patchedRoot = root
        patchedRoot.hasInProgressRevision = true
        patchedRoot.reviewGuideLifecycle = "ready"
        var patchedRevision = revision
        patchedRevision.revisionParentPrUrl = "https://github.com/org/repo/pull/9"
        model.applyEventForTest(patchReply(model: model, cards: [
            (patchedRoot, runtime("root")),
            (patchedRevision, runtime("rev", executionStatus: "running")),
        ]))

        XCTAssertEqual(model.task(withID: "root")?.hasInProgressRevision, true, "the root's derived flag follows its revision")
        XCTAssertEqual(model.task(withID: "rev")?.revisionParentPrUrl, "https://github.com/org/repo/pull/9")
        XCTAssertEqual(model.taskRuntime(for: "rev")?.executionStatus, "running")
        XCTAssertTrue(recorder.sent.isEmpty)
    }

    func testPatchThatMovesACardBetweenProjectsFallsBackAndChangesNothing() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        recorder.reset()

        var moved = makeTask(id: "task_1", projectID: "proj_a2")
        moved.ciRequiredState = "success"
        model.applyEventForTest(patchReply(model: model, cards: [(moved, runtime("task_1"))]))

        XCTAssertEqual(model.tasksByProjectID["proj_a1"]?.map(\.id), ["task_1"], "the board is left for the tree to rewrite")
        XCTAssertNil(model.task(withID: "task_1")?.ciRequiredState)
        XCTAssertEqual(recorder.counts["get_work_tree"], 1)
        XCTAssertEqual(recorder.counts["list_deferred_scope_attentions"], 1)
    }

    func testPatchThatChangesAStatusOrOrderingFallsBack() {
        let mutations: [(inout WorkTask) -> Void] = [
            { $0.status = "in_review" },
            { $0.ordinal = 7 },
            { $0.parentTaskId = "someone" },
            { $0.deletedAt = "2026-10-22T10:00:00Z" },
        ]
        for mutate in mutations {
            let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
            model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
            recorder.reset()

            var changed = makeTask(id: "task_1")
            mutate(&changed)
            model.applyEventForTest(patchReply(model: model, cards: [(changed, runtime("task_1"))]))

            XCTAssertEqual(recorder.counts["get_work_tree"], 1, "a structural field changed under a runtime push")
            XCTAssertEqual(model.task(withID: "task_1")?.status, "todo")
        }
    }

    func testPatchThatFlipsAChoreAndTaskArmFallsBack() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1", projectID: nil, kind: "chore")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        recorder.reset()

        model.applyEventForTest(patchReply(
            model: model, cards: [(makeTask(id: "task_1", projectID: nil, kind: "chore"), runtime("task_1"))],
            asChore: false
        ))

        XCTAssertEqual(recorder.counts["get_work_tree"], 1)
    }

    func testDeletedCardInAPatchFallsBack() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1"), makeTask(id: "task_2")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1", "task_2"])
        recorder.reset()

        model.applyEventForTest(patchReply(model: model, cards: [(makeTask(id: "task_1"), runtime("task_1"))], absent: ["task_2"]))

        XCTAssertEqual(recorder.counts["get_work_tree"], 1, "task_2 is held but no longer a live card")
        XCTAssertNotNil(model.task(withID: "task_2"), "the tree, not the patch, removes it")
    }

    func testAbsentIDTheAppNeverHeldIsIgnored() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1", "proj_other"])
        recorder.reset()

        model.applyEventForTest(patchReply(model: model, cards: [(makeTask(id: "task_1"), runtime("task_1"))], absent: ["proj_other"]))

        XCTAssertTrue(recorder.sent.isEmpty)
    }

    func testPatchNamingACardTheAppDoesNotHoldFallsBack() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_new"])
        recorder.reset()

        model.applyEventForTest(patchReply(model: model, cards: [(makeTask(id: "task_new"), runtime("task_new"))]))

        XCTAssertEqual(recorder.counts["get_work_tree"], 1, "a new card is a membership change")
        XCTAssertNil(model.task(withID: "task_new"))
    }

    func testRejectedPatchLeavesEveryCardAsItWas() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1"), makeTask(id: "task_2")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1", "task_2"])
        recorder.reset()

        var fine = makeTask(id: "task_1")
        fine.ciRequiredState = "success"
        var structural = makeTask(id: "task_2")
        structural.status = "done"
        model.applyEventForTest(patchReply(model: model, cards: [(fine, runtime("task_1", waitReason: "x")), (structural, runtime("task_2"))]))

        XCTAssertNil(model.task(withID: "task_1")?.ciRequiredState, "all cards are checked before any is applied")
        XCTAssertNil(model.taskRuntime(for: "task_1")?.dispatchWaitReason)
        XCTAssertEqual(recorder.counts["get_work_tree"], 1)
    }

    func testPatchForAProductNoLongerOnScreenIsDropped() {
        let (model, recorder) = makeModel(cards: [makeTask(id: "task_1")])
        model.requestRuntimePatch(productID: "prod_a", itemIDs: ["task_1"])
        model.selectWorkProduct("prod_b")
        recorder.reset()

        var patched = makeTask(id: "task_1")
        patched.ciRequiredState = "success"
        model.applyEventForTest(patchReply(model: model, cards: [(patched, runtime("task_1"))]))

        XCTAssertTrue(recorder.sent.isEmpty, "the switch fetched the new product itself")
    }

    // MARK: - Helpers

    private final class Recorder {
        var sent: [[String: Any]] = []

        var counts: [String: Int] {
            sent.reduce(into: [:]) { counts, payload in
                counts[payload["type"] as? String ?? "?", default: 0] += 1
            }
        }

        var patchBodies: [[String]] {
            sent.filter { $0["type"] as? String == "get_work_runtime_patch" }
                .compactMap { $0["item_ids"] as? [String] }
        }

        var patchItemIDs: [String] { patchBodies.last ?? [] }

        func reset() { sent.removeAll() }
    }

    private func runtimeInvalidation(_ itemIDs: [String]) -> EngineEvent {
        .workInvalidated(
            topic: "work.product.prod_a", productId: "prod_a", itemIds: itemIDs, invalidationClass: .runtime
        )
    }

    private func invalidationLine(class cls: String?, itemIDs: [String]) -> String {
        let ids = itemIDs.map { "\"\($0)\"" }.joined(separator: ",")
        let classField = cls.map { ",\"class\":\"\($0)\"" } ?? ""
        return """
        {"payload":{"type":"topic_event","topic":"work.product.prod_a","revision":5,"origin_session_id":"",\
        "event":{"type":"work_invalidated","reason":"pr_poll_state_updated","product_id":"prod_a","item_ids":[\(ids)]\(classField)}}}
        """
    }

    private func patchReply(
        model: ChatViewModel,
        cards: [(WorkTask, WorkTaskRuntime)],
        absent: [String] = [],
        asChore: Bool? = nil,
        isComplete: Bool = true
    ) -> EngineEvent {
        .workRuntimePatch(
            productId: "prod_a",
            cards: cards.map { task, runtime in
                WorkRuntimePatchCard(item: (asChore ?? task.isChore) ? .chore(task) : .task(task), runtime: runtime)
            },
            absent: absent,
            isComplete: isComplete,
            requestId: model.boardReadFences["prod_a"]?.patchRequestID
        )
    }

    private func runtime(
        _ id: String, executionStatus: String? = nil, waitReason: String? = nil
    ) -> WorkTaskRuntime {
        WorkTaskRuntime(
            workItemID: id,
            executionStatus: executionStatus,
            runStatus: nil,
            executionID: nil,
            dispatchWaitReason: waitReason,
            dispatchWaitSince: nil
        )
    }

    private func waitFor(_ recorder: Recorder, _ verb: String) async {
        await waitUntil { (recorder.counts[verb] ?? 0) > 0 }
        XCTAssertGreaterThan(recorder.counts[verb] ?? 0, 0, "no \(verb) arrived")
        // Any straggler a second burst would have sent.
        try? await Task.sleep(for: .milliseconds(150))
    }

    private func waitUntil(_ condition: () -> Bool) async {
        let deadline = ContinuousClock.now + .seconds(5)
        while !condition(), ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(5))
        }
    }

    /// A model showing `prod_a` with `cards` on its board, connected to a
    /// recorder instead of an engine. No read is outstanding.
    private func makeModel(
        cards: [WorkTask] = [],
        productLevelRevisions: [WorkTask] = []
    ) -> (ChatViewModel, Recorder) {
        let model = ChatViewModel(socketPath: "/tmp/boss-runtime-patch-\(UUID().uuidString).sock")
        model.products = [makeProduct("prod_a"), makeProduct("prod_b")]
        model.selectWorkProduct("prod_a")
        model.refetchCoalescingPolicy = .init(window: .milliseconds(40), maxWait: .seconds(2))
        model.applyEventForTest(makeWorkTreeEvent(tasks: cards + productLevelRevisions))
        model.isConnected = true
        model.engine.simulateConnectedForTesting = true
        let recorder = Recorder()
        model.outboundRecorder = { recorder.sent.append($0) }
        return (model, recorder)
    }

    private func makeProduct(_ id: String) -> WorkProduct {
        WorkProduct(
            id: id,
            name: "Product \(id)",
            slug: id,
            description: "",
            repoRemoteURL: "https://github.com/org/repo.git",
            status: "active",
            createdAt: "2026-07-10T00:00:00Z",
            updatedAt: "2026-07-10T00:00:00Z"
        )
    }

    private func makeTask(
        id: String,
        projectID: String? = "proj_a1",
        kind: String = "project_task",
        status: String = "todo"
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_a",
            projectID: projectID,
            kind: kind,
            name: "Task \(id)",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-07-10T00:00:00Z",
            updatedAt: "2026-07-10T00:00:00Z"
        )
    }

    private func makeWorkTreeEvent(tasks: [WorkTask], requestID: String? = nil, revision: UInt64? = nil) -> EngineEvent {
        let projects = ["proj_a1", "proj_a2"].map { id in
            WorkProject(
                id: id,
                productID: "prod_a",
                name: "Project \(id)",
                slug: id,
                description: "",
                goal: "",
                status: "active",
                priority: "medium",
                createdAt: "2026-07-10T00:00:00Z",
                updatedAt: "2026-07-10T00:00:00Z"
            )
        }
        let chores = tasks.filter { $0.projectID == nil && $0.kind == "chore" }
        return .workTree(
            product: makeProduct("prod_a"),
            projects: projects,
            tasks: tasks.filter { !chores.contains($0) },
            chores: chores,
            taskRuntimes: [],
            dependencies: [],
            ideas: [],
            requestId: requestID, revision: revision
        )
    }
}
