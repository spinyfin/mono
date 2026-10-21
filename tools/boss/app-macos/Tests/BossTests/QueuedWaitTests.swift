import XCTest
@testable import Boss

@MainActor
final class QueuedWaitTests: XCTestCase {
    func testRecordedReasonLabels() {
        let labels = [
            "unknown": WorkCardLiveStatus.pendingWithoutReasonLabel,
            "waiting_dependency": "Queued — waiting for a dependency",
            "dispatch_paused": "Queued — dispatch paused",
            "automation_paused": "Queued — automation paused",
            "Waiting for an available worker (3 working)":
                "Queued — Waiting for an available worker (3 working)",
            "pool_exhausted": "Queued — worker pool full",
            "blocked by an earlier review": "Queued — blocked by an earlier review"
        ]
        for (reason, label) in labels {
            XCTAssertEqual(WorkCardLiveStatus.queuedLabel(runtime: runtime(reason: reason), now: Date()), label)
        }
    }

    func testNotBeforeIncludesTheRecordedTime() {
        var pending = runtime(reason: "not_before")
        pending.dispatchNotBefore = "1900000000"
        let date = Date(timeIntervalSince1970: 1_900_000_000)
        XCTAssertEqual(
            WorkCardLiveStatus.queuedLabel(runtime: pending, now: Date()),
            "Queued — not before \(date.formatted(date: .abbreviated, time: .shortened))"
        )
        pending.dispatchNotBefore = "bad"
        XCTAssertEqual(
            WorkCardLiveStatus.queuedLabel(runtime: pending, now: Date()),
            "Queued — held until a scheduled time"
        )
    }

    /// Every hold reason the engine can state for a `todo` + `autostart`
    /// row with nothing queued maps to a specific line; none of them, and
    /// neither fallback, reads "reason unknown".
    func testEngineHoldReasonsRenderSpecificLabels() {
        let labels = [
            "human_driven": "Held — human-driven; no worker will run",
            "deferred": "Held — deferred until approved",
            "pr_awaits_review": "Held — already has a PR; file a revision to change it",
            "deliberately_parked": "Held — parked after its last run; start it to resume",
            "repo_unresolved": "Held — no repository resolved for this item",
            "awaiting_reconcile": "Queued — waiting for the engine to reconcile",
            "some_future_gate": "Held — some_future_gate"
        ]
        for (reason, label) in labels {
            var stranded = runtime(reason: nil, status: "cancelled")
            stranded.dispatchHoldReason = reason
            XCTAssertEqual(WorkCardLiveStatus.queuedLabel(runtime: stranded, now: Date()), label, reason)
            XCTAssertFalse(label.contains("reason unknown"))
        }
        XCTAssertFalse(WorkCardLiveStatus.nothingQueuedLabel.contains("reason unknown"))
        XCTAssertFalse(WorkCardLiveStatus.pendingWithoutReasonLabel.contains("reason unknown"))
    }

    /// The reported stranded shape: a `todo` + `autostart` row whose only
    /// execution is `cancelled` and for which the engine has nothing queued.
    /// Before this fix the card read "Queued — reason unknown"; now it says
    /// what the engine knows — nothing is queued — and, once the engine has
    /// stated a hold reason, that reason.
    func testStrandedCancelledRowNoLongerReadsReasonUnknown() {
        let stranded = runtime(reason: nil, status: "cancelled")
        let label = WorkCardLiveStatus.resolve(
            task: task(status: "todo"), column: .doing, runtime: stranded, liveState: nil
        )
        XCTAssertEqual(label, WorkCardLiveStatus.nothingQueuedLabel)
        XCTAssertFalse(label?.contains("reason unknown") ?? true)
        var reconciling = stranded
        reconciling.dispatchHoldReason = "awaiting_reconcile"
        XCTAssertEqual(
            WorkCardLiveStatus.resolve(task: task(status: "todo"), column: .doing, runtime: reconciling, liveState: nil),
            "Queued — waiting for the engine to reconcile"
        )
    }

    func testRetryBackoffWinsOverNotBeforeWhenBothAreSet() {
        let now = Date()
        let future = String(Int(now.timeIntervalSince1970) + 120)
        var pending = runtime(reason: "not_before")
        pending.dispatchNotBefore = future
        pending.dispatchRetryAt = future
        XCTAssertTrue(WorkCardLiveStatus.queuedLabel(runtime: pending, now: now)
            .hasPrefix("Retrying dispatch — next attempt"))
    }

    func testProjectBlockerRendersWithProjectPrefix() throws {
        let client = EngineClient(socketPath: "/tmp/queued-project-test-\(UUID().uuidString).sock")
        let pending = try XCTUnwrap(client.parseTaskRuntime([
            "work_item_id": "task_waiting", "execution_status": "waiting_dependency",
            "dispatch_wait_reason": "waiting_dependency",
            "dispatch_wait_blocker": [
                "work_item_id": "proj_blocker", "product_id": "product_test", "short_id": 7, "kind": "project"
            ]
        ]))
        XCTAssertEqual(WorkCardLiveStatus.queuedLabel(runtime: pending, now: Date()), "Queued — waiting for P7")
    }

    func testPayloadReachesExistingStatusRowAndRevealTarget() throws {
        let client = EngineClient(socketPath: "/tmp/queued-wait-test-\(UUID().uuidString).sock")
        let pending = try XCTUnwrap(client.parseTaskRuntime([
            "work_item_id": "task_waiting", "execution_status": "waiting_dependency",
            "dispatch_wait_reason": "waiting_dependency",
            "dispatch_wait_blocker": [
                "work_item_id": "task_blocker", "product_id": "product_test", "short_id": 42
            ]
        ]))
        let blocker = try XCTUnwrap(pending.dispatchWaitBlocker)
        let model = ChatViewModel(socketPath: "/tmp/queued-wait-model-\(UUID().uuidString).sock")
        let waiting = task(id: pending.workItemID, status: "todo")
        let blocking = task(id: blocker.workItemID, status: "active")
        model.choresByProductID = ["product_test": [waiting, blocking]]
        model.taskRuntimesByID = [waiting.id: pending]
        let snapshot = snapshot(model, waiting)
        let row = try XCTUnwrap(WorkBoardCardLiveStatusRowSlice(snapshot: snapshot))
        XCTAssertEqual(row.liveStatus, "Queued — waiting for T\(42)")
        XCTAssertEqual(row.blocker, blocker)
        XCTAssertEqual(model.revealCardTarget(for: blocker.workItemID), .revealed(cardID: blocking.id))

        // Reproduce a claimed execution with stale wait fields: the state wins,
        // and both the text and the clickable target disappear in the same snapshot.
        model.taskRuntimesByID[waiting.id] = runtime(
            reason: "waiting_dependency", status: "claimed", blocker: blocker
        )
        let claimed = self.snapshot(model, waiting)
        XCTAssertNotEqual(snapshot, claimed)
        XCTAssertEqual(claimed.liveStatus, "Starting worker")
        XCTAssertNil(WorkBoardCardLiveStatusRowSlice(snapshot: claimed)?.blocker)
        model.taskRuntimesByID[waiting.id] = runtime(reason: nil, status: "running")
        let running = self.snapshot(model, task(id: waiting.id, status: "active"))
        XCTAssertNil(running.liveStatus)
        XCTAssertNil(running.dispatchWaitBlocker)
    }

    func testQueuedActiveCardAndOldPayloadRemainReadable() throws {
        let pending = runtime(reason: "dispatch_paused")
        XCTAssertEqual(WorkCardLiveStatus.resolve(
            task: task(status: "active"), column: .doing, runtime: pending, liveState: nil
        ), "Queued — dispatch paused")
        let client = EngineClient(socketPath: "/tmp/queued-old-test-\(UUID().uuidString).sock")
        let old = try XCTUnwrap(client.parseTaskRuntime(["work_item_id": "task_waiting"]))
        XCTAssertNil(old.dispatchWaitBlocker)
        XCTAssertEqual(WorkCardLiveStatus.resolve(
            task: task(status: "todo"), column: .doing, runtime: old, liveState: nil
        ), WorkCardLiveStatus.nothingQueuedLabel)
        let blocker = DispatchWaitBlocker(workItemID: "task_internal", productID: "product_test", shortID: nil)
        XCTAssertEqual(WorkCardLiveStatus.queuedLabel(
            runtime: runtime(reason: "waiting_dependency", blocker: blocker), now: Date()
        ), "Queued — waiting for a dependency")
    }

    func testExistingHoldWordingIsPreserved() {
        let reason = "blocked by T\(42) 'Earlier review' on example/repo#42"
        let held = runtime(reason: reason)
        XCTAssertEqual(WorkCardLiveStatus.queuedLabel(runtime: held, now: Date()), "Queued — \(reason)")
    }

    func testQueuedTaskDoesNotReuseThePreviousTerminalExecutionsReason() {
        let stale = runtime(reason: "blocked by an earlier review", status: "completed")
        XCTAssertEqual(WorkCardLiveStatus.resolve(
            task: task(status: "todo"), column: .doing, runtime: stale, liveState: nil
        ), WorkCardLiveStatus.nothingQueuedLabel)
    }

    private func snapshot(_ model: ChatViewModel, _ task: WorkTask) -> WorkCardSnapshot {
        model.workCardSnapshot(for: task, column: .doing, isSelected: false,
                               isFrontierHighlighted: false, boardStyle: .elevated, liveState: nil)
    }

    private func runtime(reason: String?, status: String = "ready", blocker: DispatchWaitBlocker? = nil) -> WorkTaskRuntime {
        WorkTaskRuntime(workItemID: "task_waiting", executionStatus: status, runStatus: nil,
                        executionID: "exec_waiting", dispatchRetryAt: nil, dispatchWaitReason: reason,
                        dispatchWaitSince: nil, dispatchWaitBlocker: blocker)
    }

    private func task(id: String = "task_waiting", status: String) -> WorkTask {
        WorkTask(id: id, productID: "product_test", projectID: nil, kind: "chore", name: "Review revision",
                 description: "", status: status, priority: "medium", ordinal: nil, prURL: nil,
                 deletedAt: nil, createdAt: "0", updatedAt: "0", autostart: true)
    }
}
