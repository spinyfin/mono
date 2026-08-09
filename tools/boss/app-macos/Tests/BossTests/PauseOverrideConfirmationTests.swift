import XCTest
@testable import Boss

/// Tests for the pause-only forced-dispatch confirmation flow: a drag into
/// Doing that would request execution first evaluates dispatch admission
/// (`EvaluateDispatchAdmission`), then either forwards the drop unchanged,
/// offers a confirmation naming the pause, or bounces the card back — see
/// `ChatViewModel+OptimisticKanban.swift` and
/// `docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md`.
@MainActor
final class PauseOverrideConfirmationTests: XCTestCase {

    // MARK: - No pause: forwards unchanged, no confirmation

    func testNoActivePauseClearsPendingCheckWithoutConfirmation() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        let accepted = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        XCTAssertTrue(accepted)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing,
                       "the drop must still optimistically reposition the card while evaluation is in flight")

        model.applyEventForTest(.dispatchAdmissionEvaluated(admission: admission(
            workItemID: task.id, wouldDispatch: true, pauseActive: false
        )))

        XCTAssertNil(model.pendingPauseOverrideConfirmation,
                     "no pause active means nothing to confirm")
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing,
                       "the card must remain optimistically placed once forwarded")
    }

    // MARK: - Overridable pause: offers confirmation, card stays placed

    func testOverridableOperatorPauseOffersConfirmation() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        model.applyEventForTest(.dispatchAdmissionEvaluated(admission: admission(
            workItemID: task.id,
            wouldDispatch: false,
            pauseActive: true,
            overridable: true,
            reason: "investigating worker failures",
            pausedSinceEpochS: 1_700_000_000
        )))

        let confirmation = model.pendingPauseOverrideConfirmation
        XCTAssertNotNil(confirmation, "an overridable pause with no other blocker must offer a confirmation")
        XCTAssertEqual(confirmation?.taskID, task.id)
        XCTAssertEqual(confirmation?.pauseReason, "investigating worker failures")
        XCTAssertEqual(confirmation?.pausedSinceEpochS, 1_700_000_000)
        XCTAssertTrue(confirmation?.informationalBlockers.isEmpty ?? false)
        // The card stays where the drop placed it while the operator decides.
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)
    }

    func testConfirmationListsInformationalBlockers() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        var admission = admission(workItemID: task.id, wouldDispatch: false, pauseActive: true, overridable: true)
        admission.blockers = [
            DispatchAdmissionBlocker(code: "autostart_disabled", message: "autostart is disabled"),
        ]
        model.applyEventForTest(.dispatchAdmissionEvaluated(admission: admission))

        let confirmation = model.pendingPauseOverrideConfirmation
        XCTAssertNotNil(confirmation)
        XCTAssertEqual(confirmation?.informationalBlockers.map(\.code), ["autostart_disabled"])
    }

    // MARK: - Confirm: sends the override, keeps the optimistic placement

    func testConfirmClearsConfirmationAndKeepsCardPlaced() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        model.applyEventForTest(.dispatchAdmissionEvaluated(admission: admission(
            workItemID: task.id, wouldDispatch: false, pauseActive: true, overridable: true
        )))
        XCTAssertNotNil(model.pendingPauseOverrideConfirmation)

        model.confirmPauseOverrideDrag()

        XCTAssertNil(model.pendingPauseOverrideConfirmation, "confirming must dismiss the dialog")
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing,
                       "confirming must not bounce the card — it stays placed pending the engine's reply")
    }

    func testConfirmWithNoPendingConfirmationIsANoop() {
        let model = makeModel()
        // No drag, no pending confirmation.
        model.confirmPauseOverrideDrag()
        XCTAssertNil(model.pendingPauseOverrideConfirmation)
    }

    // MARK: - Cancel: bounces the card back

    func testCancelBouncesCardBackToOrigin() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog)
        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        model.applyEventForTest(.dispatchAdmissionEvaluated(admission: admission(
            workItemID: task.id, wouldDispatch: false, pauseActive: true, overridable: true
        )))
        XCTAssertNotNil(model.pendingPauseOverrideConfirmation)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)

        model.cancelPauseOverrideDrag()

        XCTAssertNil(model.pendingPauseOverrideConfirmation, "cancelling must dismiss the dialog")
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog,
                       "cancelling must bounce the card back to its origin column")
    }

    func testCancelWithNoPendingConfirmationIsANoop() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]
        model.cancelPauseOverrideDrag()
        XCTAssertNil(model.pendingPauseOverrideConfirmation)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog,
                       "cancelling with nothing pending must not touch an untouched card")
    }

    // MARK: - Refusal bounce-back: non-overridable pause, or a hard blocker

    func testNonOverridableBreakerPauseBouncesImmediately() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        model.applyEventForTest(.dispatchAdmissionEvaluated(admission: admission(
            workItemID: task.id,
            wouldDispatch: false,
            pauseActive: true,
            overridable: false,
            reason: "spawn path unhealthy"
        )))

        XCTAssertNil(model.pendingPauseOverrideConfirmation,
                     "a non-overridable pause must never offer a confirmation")
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog,
                       "a non-overridable pause must bounce the card back")
        XCTAssertNotNil(model.dragRefusalNotice, "the refusal must surface an inline notice")
    }

    func testHardBlockerBouncesImmediatelyEvenWithoutAPause() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptDrop(task.id, onColumn: .doing, group: nil)
        var evaluated = admission(workItemID: task.id, wouldDispatch: false, pauseActive: false)
        evaluated.blockers = [
            DispatchAdmissionBlocker(code: "interactive_concurrency_cap", message: "cap reached (12/12)"),
        ]
        model.applyEventForTest(.dispatchAdmissionEvaluated(admission: evaluated))

        XCTAssertNil(model.pendingPauseOverrideConfirmation)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog,
                       "a hard blocker must bounce the card back even with no pause active")
    }

    // MARK: - Stale reply: superseded by a later drag

    func testStaleReplyForASupersededDragIsIgnored() {
        let model = makeModel()
        let taskA = makeTask(status: "todo", id: "task_a")
        let taskB = makeTask(status: "todo", id: "task_b")
        model.choresByProductID = ["prod_test": [taskA, taskB]]

        _ = model.attemptDrop(taskA.id, onColumn: .doing, group: nil)
        _ = model.attemptDrop(taskB.id, onColumn: .doing, group: nil)

        // A's reply arrives after B's drag superseded the pending check.
        model.applyEventForTest(.dispatchAdmissionEvaluated(admission: admission(
            workItemID: taskA.id, wouldDispatch: false, pauseActive: true, overridable: true
        )))

        XCTAssertNil(model.pendingPauseOverrideConfirmation,
                     "a stale reply for a superseded drag must not surface a confirmation")
    }

    // MARK: - Helpers

    private func admission(
        workItemID: String,
        wouldDispatch: Bool,
        pauseActive: Bool,
        overridable: Bool = false,
        reason: String? = "test pause",
        pausedSinceEpochS: UInt64? = 1_700_000_000
    ) -> DispatchAdmission {
        DispatchAdmission(
            workItemID: workItemID,
            wouldDispatch: wouldDispatch,
            pause: DispatchPauseSnapshot(
                active: pauseActive,
                origin: pauseActive ? (overridable ? "operator" : "breaker") : nil,
                reason: pauseActive ? reason : nil,
                pausedSinceEpochS: pauseActive ? pausedSinceEpochS : nil,
                overridable: overridable
            ),
            blockers: []
        )
    }

    private func makeTask(status: String, autostart: Bool = false, id: String? = nil) -> WorkTask {
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
