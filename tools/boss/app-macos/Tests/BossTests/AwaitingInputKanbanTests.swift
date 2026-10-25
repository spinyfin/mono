import XCTest
@testable import Boss

/// Covers the "awaiting input" kanban rule: when the live worker behind
/// a Doing-lane card is paused on a permission prompt
/// (`activity == .waitingForInput`), the card's activity dot must read
/// as *waiting* — visibly distinct from a normally-running (active)
/// worker — rather than the green "active" dot that a bound slot would
/// otherwise force. The state clears on its own when the worker resumes
/// and the activity flips back to `.working`.
@MainActor
final class AwaitingInputKanbanTests: XCTestCase {

    func testWaitingForInputSurfacesAsWaitingNotActive() {
        let live = makeLiveState(activity: .waitingForInput)
        let state = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: live,
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case let .waiting(reason) = state else {
            return XCTFail("expected .waiting for a waiting-for-input worker; got \(state)")
        }
        XCTAssertTrue(
            reason.lowercased().contains("input"),
            "waiting reason should mention input; got: \(reason)"
        )
    }

    func testWorkingBoundWorkerStaysActive() {
        // A normally-running worker (bound slot, activity=working) must
        // still read as active — the awaiting-input rule must not regress
        // the common case.
        let live = makeLiveState(activity: .working)
        let state = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: live,
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case .active = state else {
            return XCTFail("expected .active for a working bound worker; got \(state)")
        }
    }

    func testWaitingForInputTakesPrecedenceOverBoundSlot() {
        // The bound-slot "active" shortcut must not pre-empt the
        // waiting-for-input check: a worker can be both bound to a slot
        // and paused on a prompt at the same time.
        let live = makeLiveState(slotId: 3, activity: .waitingForInput)
        let state = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: live,
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case .waiting = state else {
            return XCTFail("waiting-for-input must win over a bound slot; got \(state)")
        }
    }

    func testLocalMissingTmuxIdentitySurfacesAsErrored() {
        let live = makeLiveState(activity: .working, tmuxHosted: false)
        let state = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: live,
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case let .errored(reason) = state else {
            return XCTFail("expected .errored for a local worker missing tmux identity; got \(state)")
        }
        XCTAssertEqual(reason, AgentActivityState.identityUnavailableReason)
    }

    func testInitFromLiveStateMapsMissingTmuxIdentityToErrored() {
        let live = makeLiveState(activity: .working, tmuxHosted: false)
        let state = AgentActivityState(runtime: makeRuntime(), liveState: live)
        guard case let .errored(reason) = state else {
            return XCTFail("expected .errored for tmuxHosted == false; got \(state)")
        }
        XCTAssertEqual(reason, AgentActivityState.identityUnavailableReason)
    }

    func testInitFromLiveStateKeepsNilTmuxHostedWorkingWorkerActive() {
        let live = makeLiveState(activity: .working, tmuxHosted: nil)
        let state = AgentActivityState(runtime: makeRuntime(), liveState: live)
        guard case .active = state else {
            return XCTFail("expected .active for nil tmuxHosted; got \(state)")
        }
    }

    func testRemoteNilTmuxHostedWorkingWorkerStaysActive() {
        let live = makeLiveState(activity: .working, tmuxHosted: nil)
        let state = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: live,
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case .active = state else {
            return XCTFail("expected .active for a remote worker with nil tmuxHosted; got \(state)")
        }
    }

    func testSpawningBoundWorkerSurfacesAsUnknownNotActive() {
        let live = makeLiveState(activity: .spawning)
        let state = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: live,
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case let .unknown(reason) = state else {
            return XCTFail("expected .unknown for a spawning worker; got \(state)")
        }
        XCTAssertTrue(reason.lowercased().contains("not yet"), "unknown reason should explain the missing state")
    }

    func testDispatchPendingStillWins() {
        // Dispatch-pending is the highest-precedence rule and must hold
        // even if a stale live state reports waiting-for-input.
        let live = makeLiveState(activity: .waitingForInput)
        let state = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: live,
            isDispatchPending: true,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case .dispatchPending = state else {
            return XCTFail("dispatch-pending must win; got \(state)")
        }
    }

    func testResumingWorkerClearsWaiting() {
        // The same card, once the worker resumes, is no longer waiting —
        // the indicator clears automatically from the activity flip.
        let resumed = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: makeLiveState(activity: .working),
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case .active = resumed else {
            return XCTFail("resumed worker should read as active; got \(resumed)")
        }
    }

    func testMidTurnPromptWithDeclaredWaitStaysNeedsInput() {
        // The engine keeps `.waitingForInput` (and `wait_reason`) for a
        // prompt raised while the declaring turn is still running: that is
        // a real ask of the owner, not a job wait.
        let live = makeLiveState(
            activity: .waitingForInput,
            waitReason: "bazel build //...",
            waitExpiresAt: "2026-10-24T19:08:33Z"
        )
        XCTAssertFalse(live.isWaitingOnJob)
        let card = AgentActivityState.forDoingCard(
            runtime: makeRuntime(waitingHuman: true),
            liveState: live,
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        let pane = AgentActivityState(runtime: makeRuntime(waitingHuman: true), liveState: live)
        XCTAssertEqual(card, .waiting(reason: "Waiting on user input"))
        XCTAssertEqual(pane, .waiting(reason: "Waiting on user input"))
    }

    func testDeclaredWaitWithIdleActivityRendersAsWaitingWithReason() {
        let live = makeLiveState(
            activity: .idle,
            waitReason: "bazel build //... running in background",
            waitExpiresAt: "2026-10-24T19:08:33Z"
        )
        XCTAssertTrue(live.isWaitingOnJob)
        let card = AgentActivityState.forDoingCard(
            runtime: makeRuntime(waitingHuman: true),
            liveState: live,
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        let pane = AgentActivityState(runtime: makeRuntime(waitingHuman: true), liveState: live)
        for state in [card, pane] {
            guard case let .waiting(reason) = state else {
                return XCTFail("expected .waiting; got \(state)")
            }
            XCTAssertTrue(reason.hasPrefix("Waiting: bazel build //... running in background · until "), reason)
            XCTAssertFalse(reason.lowercased().contains("input"))
        }
    }

    func testBadgeLabelsSeparateJobWaitsFromNeedsInput() {
        XCTAssertEqual(
            AgentsBadge.labels(waitingOnJob: 2, needsInput: 0).map(\.text), ["Waiting 2"])
        XCTAssertEqual(
            AgentsBadge.labels(waitingOnJob: 0, needsInput: 1).map(\.text), ["Needs input 1"])
        XCTAssertEqual(
            AgentsBadge.labels(waitingOnJob: 2, needsInput: 1).map(\.text),
            ["Waiting 2", "Needs input 1"])
        XCTAssertTrue(AgentsBadge.labels(waitingOnJob: 0, needsInput: 0).isEmpty)
    }

    // MARK: - Waiting on the worker's own declared work

    func testWaitingOnWorkReadsAsWaitingWithTheDeclaredReasonNotInput() {
        let live = makeLiveState(
            activity: .waitingOnWork,
            waitReason: "bazel build of merge resolution",
            waitExpiresAt: "2026-10-21T02:26:57Z"
        )
        for state in [
            AgentActivityState.forDoingCard(
                runtime: makeRuntime(),
                liveState: live,
                isDispatchPending: false,
                isResolvingConflicts: false,
                isRemediatingCI: false
            ),
            AgentActivityState(runtime: makeRuntime(), liveState: live),
        ] {
            guard case let .waiting(reason) = state else {
                return XCTFail("expected .waiting; got \(state)")
            }
            XCTAssertTrue(reason.contains("bazel build of merge resolution"), reason)
            XCTAssertFalse(reason.lowercased().contains("input"), reason)
        }
        XCTAssertTrue(live.isWaitingOnJob)
        XCTAssertTrue(WorkerSlotLiveSlice(live).isWaitingOnJob)
        XCTAssertFalse(WorkerActivity.waitingOnWork.needsHuman)
    }

    func testWaitOverdueReadsAsWaitingAndIsNotNeedsInput() {
        let live = makeLiveState(activity: .waitOverdue, waitReason: "bazel build")
        let state = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: live,
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case let .waiting(reason) = state else {
            return XCTFail("expected .waiting; got \(state)")
        }
        XCTAssertTrue(reason.lowercased().contains("expired"), reason)
        XCTAssertTrue(live.isWaitingOnJob)
        XCTAssertTrue(WorkerSlotLiveSlice(live).isWaitingOnJob)
        XCTAssertEqual(WorkerSlotLiveSlice(live).waitCaption, reason)
        XCTAssertFalse(WorkerActivity.waitOverdue.needsHuman)
    }

    func testOnlyWaitingForInputNeedsAHuman() {
        for activity in [
            WorkerActivity.spawning, .working, .waitingOnWork, .waitOverdue, .idle, .errored, .terminated,
        ] {
            XCTAssertFalse(activity.needsHuman, "\(activity)")
        }
        XCTAssertTrue(WorkerActivity.waitingForInput.needsHuman)
    }

    func testPillLabelsNeverSayWaitingForTheNewStates() {
        XCTAssertEqual(WorkerActivity.waitingOnWork.agentsPaneLabel, "Waiting on work")
        XCTAssertEqual(WorkerActivity.waitOverdue.agentsPaneLabel, "Wait overdue")
    }

    func testSubtitleAndNeedsInputAgreeAcrossTransitions() {
        let cases: [(WorkerActivity, String?, Bool)] = [
            (.waitingOnWork, "waiting on its own work", false),
            (.waitOverdue, "wait expired — check on it", false),
            (.waitingForInput, "awaiting input", true),
            (.working, nil, false),
            (.idle, nil, false),
        ]
        for (activity, expected, needsHuman) in cases {
            for prior in ["awaiting input", "waiting on its own work", "wait expired — check on it"] {
                XCTAssertEqual(activity.slotSubtitle(prior: prior), expected)
                XCTAssertEqual(activity.needsHuman, needsHuman)
            }
            XCTAssertEqual(activity.slotSubtitle(prior: "investigating the bug"), "investigating the bug")
            XCTAssertEqual(activity.slotSubtitle(prior: nil), expected)
        }
    }

    // MARK: - Helpers

    private func makeLiveState(
        slotId: Int = 1,
        activity: WorkerActivity,
        tmuxHosted: Bool? = nil,
        waitReason: String? = nil,
        waitExpiresAt: String? = nil
    ) -> WorkerLiveState {
        WorkerLiveState(
            slotId: slotId,
            runId: "exec-1",
            model: "claude-opus-4-7",
            shellPid: 1234,
            lastEventAt: "2026-06-01T00:00:00Z",
            currentTool: nil,
            lastToolEndedAt: nil,
            activity: activity,
            liveStatus: "Waiting for user input",
            liveStatusAt: "2026-06-01T00:00:00Z",
            recoveryStatus: nil,
            tmuxHosted: tmuxHosted,
            waitReason: waitReason,
            waitExpiresAt: waitExpiresAt
        )
    }

    private func makeRuntime(waitingHuman: Bool = false) -> WorkTaskRuntime {
        WorkTaskRuntime(
            workItemID: "task_1",
            executionStatus: waitingHuman ? "waiting_human" : "running",
            runStatus: "active",
            executionID: "exec-1",
            dispatchRetryAt: nil,
            dispatchWaitReason: nil,
            dispatchWaitSince: nil
        )
    }
}
