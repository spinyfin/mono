import XCTest
@testable import Boss

@MainActor
final class QueuedWaitTests: XCTestCase {
    /// The card renders the engine's phrase as given, for a wait code and
    /// for a scheduler sentence alike. It holds no code-to-text table.
    func testEnginePhraseIsRenderedAsGiven() {
        let waits = [
            ("waiting_dependency", "waiting for a dependency"),
            ("project_order", "waiting for an earlier task in its project"),
            ("dispatch_paused", "dispatch paused"),
            ("automation_paused", "automation paused"),
            ("focus_mode", "outside the dispatch focus set"),
            ("pool_exhausted", "worker pool full"),
            ("awaiting_dispatch", "ready for the dispatcher's next pass"),
            ("Waiting for an available worker (3 working)", "Waiting for an available worker (3 working)"),
            ("Waiting for a workspace", "Waiting for a workspace"),
            ("blocked by an earlier review", "blocked by an earlier review")
        ]
        for (reason, label) in waits {
            XCTAssertEqual(
                WorkCardLiveStatus.queuedLabel(runtime: runtime(reason: reason, label: label), now: Date()),
                "Queued — \(label)"
            )
        }
    }

    /// A wait code with no phrase from the engine is never turned into text
    /// by the app: the card says the state is unexpected.
    func testAReasonWithoutAnEnginePhraseIsNotLabelledByTheApp() {
        for reason in ["pool_exhausted", "dispatch_paused", "waiting_dependency", "some_future_code"] {
            XCTAssertEqual(
                WorkCardLiveStatus.queuedLabel(runtime: runtime(reason: reason, label: nil), now: Date()),
                "Unexpected state — the engine sent no reason for this wait"
            )
        }
    }

    func testUnclassifiedWaitSaysTheStateIsUnexpected() {
        let label = "the engine has no reason recorded for this wait"
        let unclassified = runtime(reason: "unclassified", label: label, status: "waiting_dependency")
        XCTAssertEqual(
            WorkCardLiveStatus.queuedLabel(runtime: unclassified, now: Date()),
            "Unexpected state — \(label)"
        )
        XCTAssertEqual(
            WorkCardLiveStatus.nextUpLabel(runtime: unclassified, now: Date()),
            "Next up — Unexpected state — \(label)"
        )
        XCTAssertEqual(WorkCardLiveStatus.resolve(
            task: task(status: "active"), column: .doing, runtime: unclassified, liveState: nil
        ), "Unexpected state — \(label)")
    }

    /// Every queued shape, with and without an engine reason: none of them
    /// renders "unknown", and none renders an empty reason.
    func testNoQueuedCardRendersUnknown() {
        var notBefore = runtime(reason: "not_before", label: "held until its scheduled dispatch time")
        notBefore.dispatchNotBefore = "1900000000"
        var retrying = runtime(reason: "not_before", label: "held until its scheduled dispatch time")
        retrying.dispatchRetryAt = String(Int(Date().timeIntervalSince1970) + 120)
        let runtimes: [WorkTaskRuntime?] = [
            nil,
            runtime(reason: nil, label: nil),
            runtime(reason: nil, label: nil, status: "waiting_dependency"),
            runtime(reason: nil, label: nil, status: "queued"),
            runtime(reason: nil, label: nil, status: "completed"),
            runtime(reason: "unclassified", label: "the engine has no reason recorded for this wait"),
            runtime(reason: "pool_exhausted", label: "worker pool full"),
            runtime(reason: "run_ended", label: "its last run ended; start it again to queue a new run",
                    status: "cancelled"),
            notBefore,
            retrying,
            {
                var backoff = runtime(reason: "failure_backoff", label: "Retrying after error (attempt 4)")
                backoff.dispatchNotBefore = "1900000000"
                backoff.dispatchFailures = [
                    DispatchFailureAttempt(errorText: "tmux session is absent", happenedAt: "1899998200")
                ]
                return backoff
            }()
        ]
        for pending in runtimes {
            for nextUp in [false, true] {
                let line = WorkCardLiveStatus.resolve(
                    task: task(status: "todo"), column: .doing, runtime: pending, liveState: nil, isNextUp: nextUp
                )
                let text = line ?? ""
                XCTAssertFalse(text.isEmpty, "a queued card always has a status line")
                XCTAssertFalse(text.lowercased().contains("unknown"), text)
                XCTAssertFalse(text.hasSuffix("— "), text)
            }
        }
    }

    func testNotBeforeIncludesTheRecordedTime() {
        let label = "held until its scheduled dispatch time"
        var pending = runtime(reason: "not_before", label: label)
        pending.dispatchNotBefore = "1900000000"
        let date = Date(timeIntervalSince1970: 1_900_000_000)
        XCTAssertEqual(
            WorkCardLiveStatus.queuedLabel(runtime: pending, now: Date()),
            "Queued — not before \(date.formatted(date: .abbreviated, time: .shortened))"
        )
        // A deadline the app cannot parse still names the hold, in the
        // engine's words.
        pending.dispatchNotBefore = "bad"
        XCTAssertEqual(WorkCardLiveStatus.queuedLabel(runtime: pending, now: Date()), "Queued — \(label)")
    }

    /// A park after an earlier start failure may still carry a retry deadline
    /// on the wire; the card names the wait the engine classified.
    func testHostLeaseParkWithRetryDeadlineKeepsHostLeaseWording() throws {
        let client = EngineClient(socketPath: "/tmp/queued-park-\(UUID().uuidString).sock")
        let pending = try XCTUnwrap(client.parseTaskRuntime([
            "work_item_id": "task_waiting", "execution_status": "ready",
            "dispatch_wait_reason": "host_lease_park",
            "dispatch_wait_label": "waiting for the host to confirm the workspace lease",
            "dispatch_not_before": "1900000000",
            "dispatch_retry_at": "1900000000",
        ]))
        let date = Date(timeIntervalSince1970: 1_900_000_000)
        XCTAssertEqual(
            WorkCardLiveStatus.queuedLabel(runtime: pending, now: Date(timeIntervalSince1970: 1_899_999_000)),
            "Queued — waiting for the host to confirm the workspace lease — not before \(date.formatted(date: .abbreviated, time: .shortened))"
        )
    }

    func testFailureBackoffUsesEngineLabelAndFormattedTime() throws {
        let date = Date(timeIntervalSince1970: 1_900_000_000)
        var pending = runtime(reason: "failure_backoff", label: "Retrying after error (attempt 4)")
        pending.dispatchNotBefore = "1900000000"
        pending.dispatchRetryAt = "1900000000"
        pending.dispatchFailures = [
            DispatchFailureAttempt(errorText: "cube lease failed", happenedAt: "1899990000"),
            DispatchFailureAttempt(
                errorText: "tmux session is absent from list-sessions", happenedAt: "1899998200"
            ),
        ]
        XCTAssertEqual(
            WorkCardLiveStatus.queuedLabel(runtime: pending, now: Date()),
            "Retrying after error (attempt 4) — not before \(date.formatted(date: .abbreviated, time: .shortened))"
        )
        let model = ChatViewModel(socketPath: "/tmp/queued-backoff-\(UUID().uuidString).sock")
        let waiting = task(status: "todo")
        model.choresByProductID = ["product_test": [waiting]]
        model.taskRuntimesByID = [waiting.id: pending]
        let snapshot = snapshot(model, waiting)
        XCTAssertEqual(
            snapshot.liveStatus,
            "Retrying after error (attempt 4) — not before \(date.formatted(date: .abbreviated, time: .shortened))"
        )
        XCTAssertEqual(snapshot.dispatchFailures.count, 2)
        let row = try XCTUnwrap(WorkBoardCardLiveStatusRowSlice(snapshot: snapshot))
        XCTAssertEqual(row.latestFailure?.errorText, "tmux session is absent from list-sessions")
        XCTAssertEqual(row.failures.count, 2)
    }

    func testHostLeaseParkNamesTheHostWait() {
        let date = Date(timeIntervalSince1970: 1_900_000_000)
        var pending = runtime(
            reason: "host_lease_park",
            label: "waiting for the host to confirm the workspace lease"
        )
        pending.dispatchNotBefore = "1900000000"
        XCTAssertEqual(
            WorkCardLiveStatus.queuedLabel(runtime: pending, now: Date()),
            "Queued — waiting for the host to confirm the workspace lease — not before \(date.formatted(date: .abbreviated, time: .shortened))"
        )
        XCTAssertTrue(pending.dispatchFailures.isEmpty)
    }

    func testPayloadParsesDispatchFailures() throws {
        let client = EngineClient(socketPath: "/tmp/queued-failures-\(UUID().uuidString).sock")
        let pending = try XCTUnwrap(client.parseTaskRuntime([
            "work_item_id": "task_waiting", "execution_status": "ready",
            "dispatch_wait_reason": "failure_backoff",
            "dispatch_wait_label": "Retrying after error (attempt 4)",
            "dispatch_not_before": "1900000000",
            "dispatch_failures": [
                ["error_text": "first failure", "happened_at": "1000"],
                ["error_text": "tmux session is absent from list-sessions", "happened_at": "2000"],
                ["error_text": "   ", "happened_at": "3000"],
            ],
        ]))
        XCTAssertEqual(pending.dispatchFailures.map(\.errorText), [
            "first failure", "tmux session is absent from list-sessions",
        ])
        XCTAssertEqual(pending.dispatchFailures.last?.happenedAt, "2000")
        let bare = try XCTUnwrap(client.parseTaskRuntime(["work_item_id": "task_waiting"]))
        XCTAssertTrue(bare.dispatchFailures.isEmpty)
    }

    func testProjectBlockerRendersWithProjectPrefix() throws {
        let client = EngineClient(socketPath: "/tmp/queued-project-test-\(UUID().uuidString).sock")
        let pending = try XCTUnwrap(client.parseTaskRuntime([
            "work_item_id": "task_waiting", "execution_status": "waiting_dependency",
            "dispatch_wait_reason": "waiting_dependency", "dispatch_wait_label": "waiting for P7",
            "dispatch_wait_blocker": [
                "work_item_id": "proj_blocker", "product_id": "product_test", "short_id": 7, "kind": "project"
            ]
        ]))
        XCTAssertEqual(pending.dispatchWaitLabel, "waiting for P7")
        XCTAssertEqual(pending.dispatchWaitBlocker?.label, "P7")
        XCTAssertEqual(WorkCardLiveStatus.queuedLabel(runtime: pending, now: Date()), "Queued — waiting for P7")
    }

    func testPayloadReachesExistingStatusRowAndRevealTarget() throws {
        let client = EngineClient(socketPath: "/tmp/queued-wait-test-\(UUID().uuidString).sock")
        let pending = try XCTUnwrap(client.parseTaskRuntime([
            "work_item_id": "task_waiting", "execution_status": "waiting_dependency",
            "dispatch_wait_reason": "waiting_dependency", "dispatch_wait_label": "waiting for T\(42)",
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
            reason: "waiting_dependency", label: "waiting for T\(42)", status: "claimed", blocker: blocker
        )
        let claimed = self.snapshot(model, waiting)
        XCTAssertNotEqual(snapshot, claimed)
        XCTAssertEqual(claimed.liveStatus, "Starting worker")
        XCTAssertNil(WorkBoardCardLiveStatusRowSlice(snapshot: claimed)?.blocker)
        model.taskRuntimesByID[waiting.id] = runtime(reason: nil, label: nil, status: "running")
        let running = self.snapshot(model, task(id: waiting.id, status: "active"))
        XCTAssertNil(running.liveStatus)
        XCTAssertNil(running.dispatchWaitBlocker)
    }

    func testQueuedActiveCardAndAPayloadWithNoReasonRemainReadable() throws {
        let pending = runtime(reason: "dispatch_paused", label: "dispatch paused")
        XCTAssertEqual(WorkCardLiveStatus.resolve(
            task: task(status: "active"), column: .doing, runtime: pending, liveState: nil
        ), "Queued — dispatch paused")
        // A runtime that carries no reason at all (an engine that predates
        // the wait classifier) is not given one by the app.
        let client = EngineClient(socketPath: "/tmp/queued-old-test-\(UUID().uuidString).sock")
        let bare = try XCTUnwrap(client.parseTaskRuntime(["work_item_id": "task_waiting"]))
        XCTAssertNil(bare.dispatchWaitBlocker)
        XCTAssertNil(bare.dispatchWaitLabel)
        XCTAssertEqual(WorkCardLiveStatus.resolve(
            task: task(status: "todo"), column: .doing, runtime: bare, liveState: nil
        ), "Unexpected state — the engine sent no reason for this wait")
        let blocker = DispatchWaitBlocker(workItemID: "task_internal", productID: "product_test", shortID: nil)
        XCTAssertEqual(WorkCardLiveStatus.queuedLabel(
            runtime: runtime(reason: "waiting_dependency", label: "waiting for a dependency", blocker: blocker),
            now: Date()
        ), "Queued — waiting for a dependency")
    }

    func testCardSubtitleShowsDeclaredWaitIncludingCIRemediation() {
        func live(_ activity: WorkerActivity, liveStatus: String?) -> WorkerLiveState {
            WorkerLiveState(
                slotId: 1, runId: "exec-1", model: "m", shellPid: 1, lastEventAt: nil,
                currentTool: nil, lastToolEndedAt: nil, activity: activity,
                liveStatus: liveStatus, liveStatusAt: nil, recoveryStatus: nil,
                tmuxHosted: true, waitReason: "bazel test", waitExpiresAt: nil
            )
        }
        var ciTask = task(status: "blocked")
        ciTask.blockedReason = "ci_failure"
        for status in [nil, "Waiting for user input"] {
            XCTAssertEqual(WorkCardLiveStatus.resolve(
                task: ciTask, column: .doing, runtime: nil, liveState: live(.idle, liveStatus: status)
            ), "Waiting: bazel test")
            XCTAssertEqual(WorkCardLiveStatus.resolve(
                task: task(status: "active"), column: .doing, runtime: nil,
                liveState: live(.idle, liveStatus: status)
            ), "Waiting: bazel test")
        }
        // A mid-turn prompt (waitingForInput) is not a job wait.
        XCTAssertNil(WorkCardLiveStatus.resolve(
            task: ciTask, column: .doing, runtime: nil, liveState: live(.waitingForInput, liveStatus: nil)
        ))
        XCTAssertEqual(WorkCardLiveStatus.resolve(
            task: task(status: "active"), column: .doing, runtime: nil,
            liveState: live(.waitingForInput, liveStatus: "Waiting for user input")
        ), "Waiting for user input")
    }

    func testExistingHoldWordingIsPreserved() {
        let reason = "blocked by T\(42) 'Earlier review' on example/repo#42"
        let held = runtime(reason: reason, label: reason)
        XCTAssertEqual(WorkCardLiveStatus.queuedLabel(runtime: held, now: Date()), "Queued — \(reason)")
    }

    /// A card the board queues with no execution pending shows the engine's
    /// reason for that state. A finished run's hold is never reused: the
    /// engine does not send one, and the app does not keep one.
    func testQueuedCardWithNothingPendingShowsTheEnginesReason() {
        let label = "its last run ended; start it again to queue a new run"
        XCTAssertEqual(WorkCardLiveStatus.resolve(
            task: task(status: "todo"), column: .doing,
            runtime: runtime(reason: "run_ended", label: label, status: "completed"), liveState: nil
        ), "Queued — \(label)")
        XCTAssertEqual(WorkCardLiveStatus.resolve(
            task: task(status: "todo"), column: .doing,
            runtime: runtime(reason: nil, label: nil, status: "completed"), liveState: nil
        ), "Unexpected state — the engine sent no reason for this wait")
    }

    func testIdleActiveReasonsReachResolveWithTerminalOrAbsentExecution() {
        for status: String? in ["completed", "cancelled", nil] {
            for (reason, lead) in [("awaiting_redispatch", "Waiting"), ("unclassified", "Unexpected state")] {
                let pending = runtime(reason: reason, label: "engine phrase", status: status)
                XCTAssertEqual(WorkCardLiveStatus.resolve(
                    task: task(status: "active"), column: .doing, runtime: pending, liveState: nil
                ), "\(lead) — engine phrase")
            }
        }
    }

    func testReviewAndMergeReasonsReachReviewColumn() {
        for (status, reason) in [("waiting_review", "review_reservation"),
                                 ("waiting_merge", "awaiting_merge"),
                                 ("completed", "review_batch")] {
            XCTAssertEqual(WorkCardLiveStatus.resolve(
                task: task(status: "in_review"), column: .review,
                runtime: runtime(reason: reason, label: "engine phrase", status: status), liveState: nil
            ), "Waiting — engine phrase")
        }
    }

    func testReviewRuntimeReachesBoardSnapshot() {
        let model = ChatViewModel(socketPath: "/tmp/review-snapshot-\(UUID().uuidString).sock")
        let item = task(status: "in_review")
        model.choresByProductID = ["product_test": [item]]
        for reason in ["review_reservation", "review_batch", "awaiting_merge",
                       "review_complete", "revision_pending", "human_review", "unclassified"] {
            model.taskRuntimesByID[item.id] = runtime(reason: reason, label: "engine phrase", status: "completed")
            let card = model.workCardSnapshot(
                for: item, column: .review, isSelected: false,
                isFrontierHighlighted: false, boardStyle: .elevated, liveState: nil
            )
            XCTAssertEqual(card.liveStatus, "\(reason == "unclassified" ? "Unexpected state" : "Waiting") — engine phrase")
        }
    }

    func testApplyingAIReviewCaptionWinsOverIdleWait() {
        var item = task(status: "active")
        item.aiReviewing = true
        item.aiReviewStage = "applying"
        XCTAssertEqual(WorkCardLiveStatus.resolve(
            task: item, column: .doing,
            runtime: runtime(reason: "awaiting_review", label: "generic review wait", status: "completed"),
            liveState: nil
        ), "Applying AI review verdict")
    }

    func testEngineWaitDoesNotOverrideLiveOrHumanWork() {
        for status in ["claimed", "running", "waiting_human"] {
            XCTAssertFalse(WorkCardLiveStatus.isQueued(
                task: task(status: "active"),
                runtime: runtime(reason: "awaiting_redispatch", label: "stale", status: status)))
        }
        var human = task(status: "active")
        human.humanDriven = true
        XCTAssertEqual(WorkCardLiveStatus.resolve(
            task: human, column: .doing,
            runtime: runtime(reason: "awaiting_redispatch", label: "stale", status: "completed"), liveState: nil
        ), "Human-driven — waiting on you")
    }

    private func snapshot(_ model: ChatViewModel, _ task: WorkTask) -> WorkCardSnapshot {
        model.workCardSnapshot(for: task, column: .doing, isSelected: false,
                               isFrontierHighlighted: false, boardStyle: .elevated, liveState: nil)
    }

    /// `label` is the phrase the engine sends with `reason`.
    private func runtime(
        reason: String?, label: String?, status: String? = "ready", blocker: DispatchWaitBlocker? = nil
    ) -> WorkTaskRuntime {
        WorkTaskRuntime(workItemID: "task_waiting", executionStatus: status, runStatus: nil,
                        executionID: "exec_waiting", dispatchRetryAt: nil, dispatchWaitReason: reason,
                        dispatchWaitSince: nil, dispatchWaitBlocker: blocker, dispatchWaitLabel: label)
    }

    private func task(id: String = "task_waiting", status: String) -> WorkTask {
        WorkTask(id: id, productID: "product_test", projectID: nil, kind: "chore", name: "Review revision",
                 description: "", status: status, priority: "medium", ordinal: nil, prURL: nil,
                 deletedAt: nil, createdAt: "0", updatedAt: "0", autostart: true)
    }
}
