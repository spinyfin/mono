import XCTest
@testable import Boss

/// Covers Doing's "Needs Attention" section: a task a blocked worker parked
/// with a typed question (`blocked_reason = awaiting_operator_answer` plus an
/// engine-projected `operator_question`) routes to Doing, renders in its own
/// section above everything else, and is answered with Yes/No from the card.
@MainActor
final class OperatorQuestionKanbanTests: XCTestCase {

    // MARK: - Parser

    func testParseTaskDecodesOperatorQuestion() throws {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var payload = basePayload()
        payload["status"] = "blocked"
        payload["blocked_reason"] = "awaiting_operator_answer"
        payload["operator_question"] = [
            "id": "oq_1",
            "text": "Approve bypass of the 30 max file limit (48 needed)?",
            "answer_type": ["kind": "yes_no"],
            "explanation": "Sweeps 48 files.",
            "asked_at": "1790000000",
            "execution_id": "exec_1",
            "run_summary": "Prepared the migration; awaiting permission to expand its scope.",
        ] as [String: Any]

        let task = try XCTUnwrap(client.parseTask(payload))

        let question = try XCTUnwrap(task.operatorQuestion)
        XCTAssertEqual(question.id, "oq_1")
        XCTAssertEqual(question.text, "Approve bypass of the 30 max file limit (48 needed)?")
        XCTAssertEqual(question.answerType, .yesNo)
        XCTAssertEqual(question.explanation, "Sweeps 48 files.")
        XCTAssertEqual(question.askedAt, "1790000000")
        XCTAssertEqual(question.executionID, "exec_1")
        let snapshot = WorkCardSnapshot.build(task: task, context: WorkCardSnapshotContext(column: .doing))
        XCTAssertEqual(snapshot.operatorQuestion?.runSummary, "Prepared the migration; awaiting permission to expand its scope.")
        XCTAssertNotEqual(snapshot.operatorQuestion?.runSummary, question.explanation)
        XCTAssertTrue(task.isAwaitingOperatorAnswer)
    }

    func testParseTaskWithoutOperatorQuestionDecodesNil() throws {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let task = try XCTUnwrap(client.parseTask(basePayload()))
        XCTAssertNil(task.operatorQuestion)
        XCTAssertFalse(task.isAwaitingOperatorAnswer)
    }

    /// The card only has Yes/No buttons, so a question of any other answer
    /// type must not decode into a Needs Attention card nobody can answer.
    func testParseTaskDropsQuestionWithUnsupportedAnswerType() throws {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var payload = basePayload()
        payload["status"] = "blocked"
        payload["blocked_reason"] = "awaiting_operator_answer"
        payload["operator_question"] = [
            "id": "oq_1",
            "text": "Pick one",
            "answer_type": ["kind": "multiple_choice"],
            "explanation": "",
            "asked_at": "1790000000",
            "execution_id": "exec_1",
        ] as [String: Any]

        let task = try XCTUnwrap(client.parseTask(payload))

        XCTAssertNil(task.operatorQuestion)
        XCTAssertEqual(task.boardColumn, .backlog, "an unanswerable question falls back to the plain blocked card")
    }

    // MARK: - boardColumn routing

    func testBlockedAwaitingAnswerWithQuestionRoutesToDoing() {
        let task = makeTask(status: "blocked", reason: "awaiting_operator_answer", question: makeQuestion())
        XCTAssertTrue(task.isAwaitingOperatorAnswer)
        XCTAssertEqual(task.boardColumn, .doing)
    }

    func testBlockedAwaitingAnswerReasonWithoutQuestionDoesNotRouteToDoing() {
        let task = makeTask(status: "blocked", reason: "awaiting_operator_answer", question: nil)
        XCTAssertFalse(task.isAwaitingOperatorAnswer)
        XCTAssertEqual(task.boardColumn, .backlog)
    }

    /// A question that outlives the `blocked` status (the engine has moved
    /// the task on but a stale scalar is still on the client) must not route.
    func testStaleQuestionOnNonBlockedTaskDoesNotRoute() {
        let task = makeTask(status: "todo", reason: "awaiting_operator_answer", question: makeQuestion())
        XCTAssertFalse(task.isAwaitingOperatorAnswer)
        XCTAssertEqual(task.boardColumn, .backlog)
    }

    func testQuestionWithOtherBlockedReasonDoesNotRoute() {
        let task = makeTask(status: "blocked", reason: "worker_failed", question: makeQuestion())
        XCTAssertFalse(task.isAwaitingOperatorAnswer)
        XCTAssertEqual(task.boardColumn, .backlog)
    }

    func testReviewPhaseBlockedStillRoutesToReview() {
        let task = makeTask(status: "blocked", reason: "merge_conflict", question: nil)
        XCTAssertEqual(task.boardColumn, .review)
    }

    func testBlockedBadgeLabelsNeedsAnswer() {
        XCTAssertEqual(WorkBlockedBadge.label(forReason: "awaiting_operator_answer"), "Needs Answer")
        let task = makeTask(status: "blocked", reason: "awaiting_operator_answer", question: makeQuestion())
        XCTAssertEqual(WorkBlockedBadge.badgeText(for: task), "Needs Answer")
        // The tooltip carries `blocked_detail` — the question text.
        XCTAssertEqual(
            WorkBlockedBadge.badgeTooltip(for: task),
            "Approve bypass of the 30 max file limit (48 needed)?"
        )
    }

    // MARK: - needsAttentionSection

    func testNeedsAttentionSectionIsNilForEmptyItems() {
        XCTAssertNil(ChatViewModel.needsAttentionSection(items: []))
    }

    func testNeedsAttentionSectionShape() throws {
        let section = try XCTUnwrap(ChatViewModel.needsAttentionSection(items: [awaiting(id: "task_a")]))
        XCTAssertEqual(section.id, "doing-needs-attention")
        XCTAssertEqual(section.title, "Needs Attention")
        XCTAssertTrue(section.isCollapsible)
        XCTAssertTrue(section.defaultExpanded)
        XCTAssertEqual(section.groupKey, .needsAttention)
    }

    func testNeedsAttentionSectionOrdersOldestQuestionFirst() throws {
        let newest = awaiting(id: "task_a", askedAt: "1790000300")
        let oldest = awaiting(id: "task_c", askedAt: "1790000100")
        let middle = awaiting(id: "task_b", askedAt: "1790000200")

        let section = try XCTUnwrap(ChatViewModel.needsAttentionSection(items: [newest, oldest, middle]))

        XCTAssertEqual(section.items.map(\.id), ["task_c", "task_b", "task_a"])
    }

    func testNeedsAttentionSectionComparesInstantsAcrossTimestampFormats() throws {
        // 2026-10-05T00:00:00Z is epoch 1791158400; the RFC 3339 row is
        // earlier than the epoch row despite sorting later as a string.
        let epoch = awaiting(id: "task_a", askedAt: "1791158500")
        let rfc = awaiting(id: "task_b", askedAt: "2026-10-05T00:00:00Z")

        let section = try XCTUnwrap(ChatViewModel.needsAttentionSection(items: [epoch, rfc]))

        XCTAssertEqual(section.items.map(\.id), ["task_b", "task_a"])
    }

    func testNeedsAttentionSectionBreaksTiesAndSortsUnparseableLastByTaskID() throws {
        let tieB = awaiting(id: "task_b", askedAt: "1790000100")
        let tieA = awaiting(id: "task_a", askedAt: "1790000100")
        let garbage = awaiting(id: "task_0", askedAt: "not-a-time")

        let section = try XCTUnwrap(ChatViewModel.needsAttentionSection(items: [garbage, tieB, tieA]))

        XCTAssertEqual(section.items.map(\.id), ["task_a", "task_b", "task_0"])
    }

    // MARK: - workSections(in: .doing)

    func testDoingFlatGroupingPutsNeedsAttentionAboveRemainingCards() {
        let model = makeModel()
        model.choresByProductID = ["prod_test": [
            awaiting(id: "task_q"),
            makeTask(id: "task_active", status: "active", reason: nil, question: nil),
        ]]

        let sections = model.workSections(in: .doing)

        XCTAssertEqual(sections.map(\.title), ["Needs Attention", "Doing"])
        XCTAssertEqual(sections[0].items.map(\.id), ["task_q"])
        XCTAssertEqual(sections[1].items.map(\.id), ["task_active"])
    }

    func testDoingSectionIsOmittedWhenNoTaskIsAwaitingAnAnswer() {
        let model = makeModel()
        model.choresByProductID = ["prod_test": [
            makeTask(id: "task_active", status: "active", reason: nil, question: nil),
            makeTask(id: "task_noq", status: "blocked", reason: "awaiting_operator_answer", question: nil),
        ]]

        let sections = model.workSections(in: .doing)

        XCTAssertFalse(sections.contains { $0.title == "Needs Attention" })
        XCTAssertEqual(sections.flatMap(\.items).map(\.id), ["task_active"])
    }

    func testDoingWithOnlyAQuestionRendersJustTheNeedsAttentionSection() {
        let model = makeModel()
        model.choresByProductID = ["prod_test": [awaiting(id: "task_q")]]

        let sections = model.workSections(in: .doing)

        XCTAssertEqual(sections.map(\.title), ["Needs Attention"])
    }

    func testDoingProjectGroupingKeepsNeedsAttentionAboveProjectGroups() {
        let model = makeModel()
        model.workBoardGrouping = .project
        model.choresByProductID = ["prod_test": [
            makeTask(id: "task_active", status: "active", reason: nil, question: nil),
            awaiting(id: "task_q"),
        ]]

        let sections = model.workSections(in: .doing)

        XCTAssertEqual(sections.first?.title, "Needs Attention")
        XCTAssertEqual(sections.first?.items.map(\.id), ["task_q"])
        XCTAssertEqual(sections.dropFirst().map(\.title), ["No Project"])
        XCTAssertEqual(
            sections.dropFirst().flatMap(\.items).map(\.id), ["task_active"],
            "the awaiting task must not also appear in a project group"
        )
    }

    func testBoardGroupMapsAwaitingTaskToNeedsAttention() {
        let model = makeModel()
        let task = awaiting(id: "task_q")
        let active = makeTask(id: "task_active", status: "active", reason: nil, question: nil)
        model.choresByProductID = ["prod_test": [task, active]]

        XCTAssertEqual(model.boardGroup(for: task), .needsAttention)
        XCTAssertNil(model.boardGroup(for: active))
    }

    // MARK: - Wire

    /// The engine's `BoardGroup` has no Needs Attention member, so a drop on
    /// the section must name only the column.
    func testMoveRequestNeverSendsTheClientOnlyNeedsAttentionGroup() {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var sent: [[String: Any]] = []
        client.outboundRecorder = { sent.append($0) }

        client.sendMoveWorkItemOnBoard(id: "task_a", column: .doing, group: .needsAttention)
        client.sendMoveWorkItemOnBoard(id: "task_a", column: .done, group: .merging)

        let targets = sent.compactMap { $0["target"] as? [String: Any] }
        XCTAssertEqual(targets.count, 2)
        XCTAssertEqual(targets[0]["column"] as? String, "doing")
        XCTAssertNil(targets[0]["group"])
        XCTAssertEqual(targets[1]["group"] as? String, "merging")
    }

    func testAnswerOperatorQuestionRequestEncoding() throws {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var sent: [[String: Any]] = []
        client.outboundRecorder = { sent.append($0) }

        client.sendAnswerOperatorQuestion(id: "oq_1", answer: .yesNo(true))
        client.sendAnswerOperatorQuestion(id: "task_9", answer: .yesNo(false))

        XCTAssertEqual(sent.count, 2)
        for payload in sent {
            XCTAssertEqual(payload["type"] as? String, "answer_operator_question")
        }
        XCTAssertEqual(sent[0]["id"] as? String, "oq_1")
        let yes = try XCTUnwrap(sent[0]["answer"] as? [String: Any])
        XCTAssertEqual(yes["kind"] as? String, "yes_no")
        XCTAssertEqual(yes["value"] as? Bool, true)
        XCTAssertEqual(sent[1]["id"] as? String, "task_9")
        let no = try XCTUnwrap(sent[1]["answer"] as? [String: Any])
        XCTAssertEqual(no["value"] as? Bool, false)

        // The exact JSON the engine's serde `OperatorAnswer` accepts.
        let data = try JSONSerialization.data(withJSONObject: sent[0], options: [.sortedKeys])
        XCTAssertEqual(
            String(decoding: data, as: UTF8.self),
            #"{"answer":{"kind":"yes_no","value":true},"id":"oq_1","type":"answer_operator_question"}"#
        )
    }

    func testOperatorQuestionErrorEventCarriesTheRefusalAndRequestID() {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let delivered = expectation(description: "operator_question_error delivered")
        client.onEvent = { event in
            guard case .operatorQuestionError(let message, let requestId) = event else {
                XCTFail("expected operatorQuestionError, got \(event)")
                return
            }
            XCTAssertEqual(requestId, "req-1")
            XCTAssertEqual(
                message,
                "This question can no longer be answered (it was answered). Recorded answer: Yes."
            )
            delivered.fulfill()
        }

        client.consumeLineForTesting("""
        {"request_id":"req-1","payload":{"type":"operator_question_error","error":{"code":"conflict","state":"answered","answer":{"kind":"yes_no","value":true}}}}
        """)

        wait(for: [delivered], timeout: 2)
    }

    func testOperatorQuestionFailureMessages() {
        XCTAssertEqual(
            OperatorQuestionFailure.message(from: ["code": "not_found"]),
            "This question no longer exists."
        )
        XCTAssertEqual(
            OperatorQuestionFailure.message(from: ["code": "validation_failed", "message": "kind mismatch"]),
            "The engine rejected the answer: kind mismatch"
        )
        XCTAssertEqual(OperatorQuestionFailure.message(from: nil), "The engine refused the answer.")
    }

    // MARK: - Answer flow (view model)

    func testAnswerIsRefusedWithoutAnOpenQuestion() {
        let model = makeModel()
        let plain = makeTask(id: "task_plain", status: "blocked", reason: "worker_failed", question: nil)

        XCTAssertFalse(model.answerOperatorQuestion(for: plain, answer: .yesNo(true)))
        XCTAssertTrue(model.operatorAnswerInFlightByTaskID.isEmpty)
    }

    func testAnswerWhileDisconnectedSurfacesAnErrorOnTheCard() {
        let model = makeModel()
        let task = awaiting(id: "task_q")
        model.isConnected = false

        XCTAssertFalse(model.answerOperatorQuestion(for: task, answer: .yesNo(true)))

        XCTAssertEqual(
            model.operatorAnswerErrorByTaskID["task_q"],
            "Not connected to the engine — reconnect and try again."
        )
        XCTAssertNil(model.operatorAnswerInFlightByTaskID["task_q"])
    }

    func testSecondClickIsIgnoredWhileAnAnswerIsInFlight() {
        let model = makeModel()
        let task = awaiting(id: "task_q")
        model.isConnected = true
        model.operatorAnswerInFlightByTaskID["task_q"] = "req-1"
        var sent = 0
        model.engine.outboundRecorder = { _ in sent += 1 }

        XCTAssertFalse(model.answerOperatorQuestion(for: task, answer: .yesNo(false)))

        XCTAssertEqual(sent, 0)
    }

    func testRefusalReEnablesTheCardAndShowsWhy() {
        let model = makeModel()
        model.operatorAnswerInFlightByTaskID["task_q"] = "req-1"

        model.handleOperatorQuestionError(message: "This question no longer exists.", requestId: "req-1")

        XCTAssertNil(model.operatorAnswerInFlightByTaskID["task_q"])
        XCTAssertEqual(model.operatorAnswerErrorByTaskID["task_q"], "This question no longer exists.")
    }

    func testWorkItemUpdatedSettlesTheInFlightAnswer() {
        let model = makeModel()
        let task = awaiting(id: "task_q")
        model.choresByProductID = ["prod_test": [task]]
        model.operatorAnswerInFlightByTaskID["task_q"] = "req-1"
        model.operatorAnswerErrorByTaskID["task_q"] = "stale"

        var answered = task
        answered.status = "todo"
        answered.blockedReason = nil
        answered.operatorQuestion = nil
        answered.autostart = true
        model.applyEventForTest(.workItemUpdated(item: .chore(answered)))

        XCTAssertNil(model.operatorAnswerInFlightByTaskID["task_q"])
        XCTAssertNil(model.operatorAnswerErrorByTaskID["task_q"])
        XCTAssertEqual(model.task(withID: "task_q")?.boardColumn, .doing)
        XCTAssertFalse(model.workSections(in: .doing).contains { $0.title == "Needs Attention" })
    }

    func testUnrelatedUpdateKeepsAnswerPendingUntilItsRefusalArrives() {
        let model = makeModel()
        let task = awaiting(id: "task_q")
        model.choresByProductID = ["prod_test": [task]]
        model.isConnected = true
        model.operatorAnswerInFlightByTaskID[task.id] = "req-1"
        var updated = task
        updated.name = "Renamed while answering"
        model.applyEventForTest(.workItemUpdated(item: .chore(updated)))

        XCTAssertEqual(model.operatorAnswerInFlightByTaskID[task.id], "req-1")
        XCTAssertFalse(model.answerOperatorQuestion(for: updated, answer: .yesNo(false)))
        model.applyEventForTest(.operatorQuestionError(message: "Answer refused", requestId: "req-1"))
        XCTAssertEqual(model.operatorAnswerErrorByTaskID[task.id], "Answer refused")
        XCTAssertNil(model.operatorAnswerInFlightByTaskID[task.id])
        model.applyEventForTest(.workItemUpdated(item: .chore(updated)))
        XCTAssertEqual(model.operatorAnswerErrorByTaskID[task.id], "Answer refused")
    }

    func testChangedQuestionSettlesOldAnswer() {
        let model = makeModel()
        var task = awaiting(id: "task_q")
        model.choresByProductID = ["prod_test": [task]]
        model.operatorAnswerInFlightByTaskID[task.id] = "req-1"
        task.operatorQuestion = OperatorQuestion(
            id: "oq_new", text: "Approve new scope?", answerType: .yesNo,
            explanation: "Scope changed", askedAt: "1790000100", executionID: "exec_2"
        )
        model.applyEventForTest(.workItemUpdated(item: .chore(task)))
        XCTAssertNil(model.operatorAnswerInFlightByTaskID[task.id])
    }

    func testDoingDropOnNeedsAttentionSendsNothing() {
        let model = makeModel()
        let task = makeTask(id: "task_active", status: "active", reason: nil, question: nil)
        model.choresByProductID = ["prod_test": [task]]
        var sent: [[String: Any]] = []
        model.engine.outboundRecorder = { sent.append($0) }

        XCTAssertTrue(model.attemptDrop(task.id, onColumn: .doing, group: .needsAttention))
        XCTAssertTrue(sent.isEmpty)
        XCTAssertNil(model.optimisticColumnByTaskID[task.id])
        XCTAssertNil(model.pendingDragAdmissionCheck)
    }

    func testUnattributedErrorWhileAnsweringDoesNotBlameViewer() {
        let model = makeModel()
        model.operatorAnswerInFlightByTaskID["task_q"] = "req-1"
        model.executionsInFlightTaskIDs.insert("task_viewer")
        model.attachmentsInFlightTaskIDs.insert("task_viewer")

        model.applyEventForTest(.workError(message: "Answer request failed", requestId: nil))

        XCTAssertEqual(model.executionsLoadFailureByTaskID["task_viewer"], "Loading failed. Retry?")
        XCTAssertEqual(model.attachmentsLoadFailureByTaskID["task_viewer"], "Loading failed. Retry?")
        XCTAssertEqual(model.operatorAnswerInFlightByTaskID["task_q"], "req-1")
    }

    // MARK: - Card snapshot

    func testMatchingAnswerErrorSettlesAnswerWithoutBlamingViewer() {
        let model = makeModel()
        model.operatorAnswerInFlightByTaskID["task_q"] = "req-1"
        model.executionsInFlightTaskIDs.insert("task_viewer")
        model.attachmentsInFlightTaskIDs.insert("task_viewer")

        model.applyEventForTest(.workError(message: "Answer request failed", requestId: "req-1"))

        XCTAssertNil(model.operatorAnswerInFlightByTaskID["task_q"])
        XCTAssertEqual(model.executionsLoadFailureByTaskID["task_viewer"], "Loading failed. Retry?")
        XCTAssertEqual(model.attachmentsLoadFailureByTaskID["task_viewer"], "Loading failed. Retry?")
        XCTAssertEqual(model.workErrorMessage, "Answer request failed")
    }

    func testSnapshotCarriesTheQuestionForADoingCardOnly() throws {
        let task = awaiting(id: "task_q")

        let doing = WorkCardSnapshot.build(task: task, context: WorkCardSnapshotContext(column: .doing))
        let backlog = WorkCardSnapshot.build(task: task, context: WorkCardSnapshotContext(column: .backlog))

        let presentation = try XCTUnwrap(doing.operatorQuestion)
        XCTAssertEqual(presentation.text, "Approve bypass of the 30 max file limit (48 needed)?")
        XCTAssertEqual(presentation.explanation, "Sweeps 48 files.")
        XCTAssertFalse(presentation.answerInFlight)
        XCTAssertNil(presentation.errorMessage)
        XCTAssertNil(backlog.operatorQuestion)
    }

    func testSnapshotReflectsInFlightAnswerAndRefusal() throws {
        let task = awaiting(id: "task_q")
        let idle = WorkCardSnapshot.build(task: task, context: WorkCardSnapshotContext(column: .doing))
        let inFlight = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .doing, operatorAnswerInFlight: true)
        )
        let refused = WorkCardSnapshot.build(
            task: task,
            context: WorkCardSnapshotContext(column: .doing, operatorAnswerError: "nope")
        )

        XCTAssertEqual(try XCTUnwrap(inFlight.operatorQuestion).answerInFlight, true)
        XCTAssertEqual(try XCTUnwrap(refused.operatorQuestion).errorMessage, "nope")
        // Equatable surface: each state must re-render the card.
        XCTAssertNotEqual(idle, inFlight)
        XCTAssertNotEqual(idle, refused)
    }

    func testOrdinaryDoingCardHasNoQuestionBlock() {
        let active = makeTask(id: "task_active", status: "active", reason: nil, question: nil)
        let snapshot = WorkCardSnapshot.build(task: active, context: WorkCardSnapshotContext(column: .doing))
        XCTAssertNil(snapshot.operatorQuestion)
        XCTAssertNil(WorkBoardCardOperatorQuestionSlice(snapshot: snapshot))
    }

    // MARK: - Helpers

    private func basePayload() -> [String: Any] {
        [
            "id": "task_parse",
            "product_id": "prod_test",
            "kind": "task",
            "name": "Parse me",
            "description": "",
            "status": "todo",
            "created_at": "2026-10-05T00:00:00Z",
            "updated_at": "2026-10-05T00:00:00Z",
        ]
    }

    private func makeQuestion(askedAt: String = "1790000000") -> OperatorQuestion {
        OperatorQuestion(
            id: "oq_1",
            text: "Approve bypass of the 30 max file limit (48 needed)?",
            answerType: .yesNo,
            explanation: "Sweeps 48 files.",
            askedAt: askedAt,
            executionID: "exec_1"
        )
    }

    private func awaiting(id: String, askedAt: String = "1790000000") -> WorkTask {
        makeTask(
            id: id,
            status: "blocked",
            reason: "awaiting_operator_answer",
            question: makeQuestion(askedAt: askedAt)
        )
    }

    private func makeTask(
        id: String = "task_\(UUID().uuidString)",
        status: String,
        reason: String?,
        question: OperatorQuestion?
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
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-10-05T00:00:00Z",
            updatedAt: "2026-10-05T00:00:00Z",
            autostart: false
        )
        task.blockedReason = reason
        task.blockedDetail = question?.text
        task.operatorQuestion = question
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
                createdAt: "2026-10-05T00:00:00Z",
                updatedAt: "2026-10-05T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = "prod_test"
        return model
    }
}
