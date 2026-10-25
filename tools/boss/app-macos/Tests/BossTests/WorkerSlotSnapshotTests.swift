import PaneLayout
import XCTest
@testable import Boss

/// Unit tests for `WorkerSlotSnapshot` and `WorkerSlotView` / `WorkersDetailView`
/// Equatable surfaces. Same gate shape as `WorkCardSnapshotTests`:
/// unused `WorkerLiveState` fields preserve snapshot equality; every
/// rendered field flips it; view `==` tracks the snapshot and ignores
/// closures so `.equatable()` can skip unchanged slot bodies.
@MainActor
final class WorkerSlotSnapshotTests: XCTestCase {

    func testEnginePersonaNamesAndPortraits() throws {
        let client = EngineClient(socketPath: "/tmp/persona-test-\(UUID().uuidString).sock")
        let cases: [(String?, String, Bool)] = [
            ("Worf", "Worf", true),
            ("Kirk", "Kirk", true),
            ("Ensign 41", "Ensign 41", false),
            ("Data (Remote)", "Data (Remote)", true),
            (nil, "Worker run-persona", false),
            ("", "Worker run-persona", false),
        ]
        let store = LiveWorkerStateStore()
        var previous: WorkerSlotSnapshot?
        for (name, expected, hasPortrait) in cases {
            var payload: [String: Any] = [
                "slot_id": 1, "run_id": "run-persona",
                "model": "opus", "activity": "working",
            ]
            payload["name"] = name
            let live = try XCTUnwrap(client.parseWorkerLiveState(payload))
            store.update(states: [live])
            XCTAssertEqual(store.byRunID["run-persona"]?.displayName, expected)
            let snapshot = WorkerSlotSnapshot.build(
                slot: WorkerSlot(slotId: 1, runId: "run-persona"),
                liveState: live, liveStatusEnabled: true
            )
            XCTAssertEqual(snapshot.displayName, expected)
            XCTAssertEqual(TrekPortraitGrid.cell(forPersona: snapshot.live?.name) != nil, hasPortrait)
            if let previous { XCTAssertNotEqual(previous, snapshot) }
            previous = snapshot
        }
        let fallback = WorkerSlotSnapshot.build(
            slot: WorkerSlot(slotId: 9, runId: "run-persona"),
            liveState: nil, liveStatusEnabled: true
        )
        XCTAssertEqual(fallback.displayName, "Worker run-persona")
    }

    func testPersonaFollowsTheRunNotTheSlot() throws {
        let client = EngineClient(socketPath: "/tmp/persona-reuse-\(UUID().uuidString).sock")
        func live(run: String, name: String?) throws -> WorkerLiveState {
            var payload: [String: Any] = [
                "slot_id": 1, "run_id": run, "model": "opus", "activity": "working",
            ]
            payload["name"] = name
            return try XCTUnwrap(client.parseWorkerLiveState(payload))
        }
        let store = LiveWorkerStateStore()
        let task = WorkTask(
            id: "task-persona", productID: "product-persona", projectID: nil,
            kind: "task", name: "Reuse a worker slot", description: "",
            status: "active", priority: "medium", ordinal: nil, prURL: nil,
            deletedAt: nil, createdAt: "", updatedAt: ""
        )
        let cases: [(String, String?, String)] = [
            ("run-a", "Worf", "Worf"),
            ("run-b", "Kirk", "Kirk"),
            ("run-c", nil, "Worker run-c"),
        ]
        for (run, name, expected) in cases {
            // Each update replaces the previous occupant of the same slot.
            store.update(states: [try live(run: run, name: name)])
            XCTAssertEqual(Set(store.byRunID.keys), [run])
            let current = try XCTUnwrap(store.byRunID[run])
            let pane = WorkerSlotSnapshot.build(
                slot: WorkerSlot(slotId: 1, runId: run),
                liveState: current, liveStatusEnabled: true
            )
            let card = WorkCardSnapshot.build(
                task: task,
                context: WorkCardSnapshotContext(column: .doing, liveState: current)
            )
            let title = WorkBoardCardTitleRowSlice(snapshot: card)
            XCTAssertEqual(pane.displayName, expected)
            XCTAssertEqual(title.assignedWorkerName, expected)
            XCTAssertEqual(title.assignedSlotId, 1)
            if name == nil {
                XCTAssertNil(TrekPortraitGrid.cell(forPersona: pane.live?.name))
                XCTAssertNil(TrekIconAssets.portrait(forPersona: pane.displayName))
                XCTAssertNil(TrekIconAssets.portrait(forPersona: title.assignedWorkerName))
            }
        }
    }

    func testTerminalTitleUsesPersonaAndIgnoresShellTitle() {
        let session = TerminalPaneSession(
            id: "run-example", role: .worker(slot: 1),
            launchSpec: TerminalLaunchSpec(
                fontSize: 10, workingDirectory: "/tmp", initialInput: ""
            )
        )
        XCTAssertEqual(session.displayTitle, "Worker example")
        session.setWorkerName("Data (Remote)")
        session.setTitle("Riker")
        XCTAssertEqual(session.displayTitle, "Data (Remote)")
        session.setTitle("")
        XCTAssertEqual(session.displayTitle, "Data (Remote)")
    }

    // MARK: - Exhaustive WorkerLiveState stored-property classification

    /// Fields `WorkerSlotLiveSlice` reads. Each must have a case in
    /// `testEveryRenderedLiveStateFieldFlipsSnapshotEquality`.
    private static let renderedLiveStateFieldNames: Set<String> = [
        "name",
        "activity",
        "liveStatus",
        "recoveryStatus",
        "lastEventAt",
        "waitReason",
        "waitExpiresAt",
        "agentType",
        "reviewStage",
        "projectName",
        "pool",
        "tmuxHosted",
        "viewerAttachError",
    ]

    /// `WorkerLiveState` fields the slot snapshot intentionally ignores.
    /// Changing any of these alone must not flip snapshot equality.
    private static let nonRenderedLiveStateFieldNames: Set<String> = [
        "slotId",
        "runId",
        "model",
        "shellPid",
        "currentTool",
        "lastToolEndedAt",
        "liveStatusAt",
        "projectId",
        "hostId",
        "startedAt",
    ]

    func testWorkerLiveStateStoredPropertyClassificationIsExhaustive() {
        let mirrored = Set(
            Mirror(reflecting: Self.makeLiveState()).children.compactMap(\.label)
        )
        let classified = Self.renderedLiveStateFieldNames
            .union(Self.nonRenderedLiveStateFieldNames)
        let overlap = Self.renderedLiveStateFieldNames
            .intersection(Self.nonRenderedLiveStateFieldNames)

        XCTAssertTrue(
            overlap.isEmpty,
            "field(s) listed as both rendered and non-rendered: \(overlap.sorted())"
        )
        XCTAssertEqual(
            mirrored.subtracting(classified).sorted(),
            [],
            """
            WorkerLiveState gained stored properties not classified for \
            WorkerSlotSnapshot. Add each to renderedLiveStateFieldNames \
            (and a flip-equality case) or nonRenderedLiveStateFieldNames \
            (and the non-rendered equality case). Unclassified: \
            \(mirrored.subtracting(classified).sorted())
            """
        )
        XCTAssertEqual(
            classified.subtracting(mirrored).sorted(),
            [],
            """
            Classification lists reference properties Mirror no longer sees \
            on WorkerLiveState (renamed/removed?): \
            \(classified.subtracting(mirrored).sorted())
            """
        )
    }

    // MARK: - Snapshot equality

    func testIdenticalInputsProduceEqualSnapshots() {
        let slot = WorkerSlot(slotId: 1)
        let live = Self.makeLiveState()
        let a = WorkerSlotSnapshot.build(slot: slot, liveState: live, liveStatusEnabled: true)
        let b = WorkerSlotSnapshot.build(slot: slot, liveState: live, liveStatusEnabled: true)
        XCTAssertEqual(a, b)
    }

    func testNonRenderedLiveStateFieldsPreserveEquality() {
        let slot = WorkerSlot(slotId: 1, runId: "exec-1")
        let base = Self.makeLiveState()
        let baseSnap = WorkerSlotSnapshot.build(
            slot: slot,
            liveState: base,
            liveStatusEnabled: true
        )

        let variants: [WorkerLiveState] = [
            Self.makeLiveState(slotId: 9),
            Self.makeLiveState(runId: "exec-other"),
            Self.makeLiveState(model: "other-model"),
            Self.makeLiveState(shellPid: 99),
            Self.makeLiveState(currentTool: "Bash"),
            Self.makeLiveState(lastToolEndedAt: "2026-09-01T00:00:00Z"),
            Self.makeLiveState(liveStatusAt: "2026-09-01T00:00:00Z"),
            Self.makeLiveState(projectId: "proj-other"),
            Self.makeLiveState(hostId: "remote-1"),
            Self.makeLiveState(startedAt: "2026-01-01T00:00:00Z"),
        ]
        for other in variants {
            let otherSnap = WorkerSlotSnapshot.build(
                slot: slot,
                liveState: other,
                liveStatusEnabled: true
            )
            XCTAssertEqual(
                baseSnap,
                otherSnap,
                "unused live-state field flipped snapshot equality: \(other)"
            )
        }
    }

    func testDeclaredWaitIsParsedAndCaptionedOnlyWhileIdle() throws {
        let client = EngineClient(socketPath: "/tmp/wait-test-\(UUID().uuidString).sock")
        let payload: [String: Any] = [
            "slot_id": 1, "run_id": "run-wait", "model": "opus", "activity": "idle",
            "wait_reason": "bazel test", "wait_expires_at": "2026-10-09T12:00:00Z",
        ]
        let live = try XCTUnwrap(client.parseWorkerLiveState(payload))
        XCTAssertEqual(live.waitReason, "bazel test")
        XCTAssertEqual(live.waitExpiresAt, "2026-10-09T12:00:00Z")

        // The clock time is rendered in the viewer's locale and zone, so
        // assert the contract rather than a fixed string.
        let states = [
            AgentActivityState(runtime: nil, liveState: live),
            AgentActivityState.forDoingCard(
                runtime: nil, liveState: live, isDispatchPending: false,
                isResolvingConflicts: false, isRemediatingCI: false
            ),
        ]
        for state in states {
            guard case let .waiting(reason) = state else {
                return XCTFail("expected .waiting; got \(state)")
            }
            XCTAssertTrue(reason.hasPrefix("Waiting: bazel test · until "), reason)
            XCTAssertFalse(reason.contains("2026-10-09T12:00:00Z"), reason)
        }

        // A worker that resumed before the wait expired is active again.
        let resumed = Self.makeLiveState(
            activity: .working, waitReason: "bazel test", waitExpiresAt: "2026-10-09T12:00:00Z"
        )
        XCTAssertEqual(
            AgentActivityState.forDoingCard(
                runtime: nil, liveState: resumed, isDispatchPending: false,
                isResolvingConflicts: false, isRemediatingCI: false
            ),
            .active
        )
    }

    func testPaneSnapshotShowsJobWaitForNormalizedIdle() {
        let slot = WorkerSlot(slotId: 1, runId: "exec-1")
        let idleWait = Self.makeLiveState(
            activity: .idle, liveStatus: nil,
            waitReason: "bazel test", waitExpiresAt: "2026-10-09T12:00:00Z"
        )
        let snap = WorkerSlotSnapshot.build(slot: slot, liveState: idleWait, liveStatusEnabled: true)
        XCTAssertEqual(snap.statusPillLabel, "Waiting")
        let caption = snap.live?.waitCaption
        XCTAssertTrue(caption?.hasPrefix("Waiting: bazel test · until ") == true, caption ?? "nil")

        // A prompt raised mid-turn keeps `.waitingForInput`: still the
        // owner's, so no wait caption even though the reason is carried.
        let midTurn = Self.makeLiveState(activity: .waitingForInput, waitReason: "bazel test")
        let midSnap = WorkerSlotSnapshot.build(slot: slot, liveState: midTurn, liveStatusEnabled: true)
        XCTAssertEqual(midSnap.statusPillLabel, "Waiting")
        XCTAssertNil(midSnap.live?.waitCaption)
        XCTAssertFalse(midSnap.live?.isWaitingOnJob ?? true)

        // A resumed worker with a leftover wait is not waiting.
        let resumed = Self.makeLiveState(activity: .working, waitReason: "bazel test")
        let resumedSnap = WorkerSlotSnapshot.build(slot: slot, liveState: resumed, liveStatusEnabled: true)
        XCTAssertNil(resumedSnap.live?.waitCaption)
        XCTAssertEqual(resumedSnap.statusPillLabel, "Working")
    }

    func testRenderedLiveStateFieldsFlipEquality() {
        let slot = WorkerSlot(slotId: 1, runId: "exec-1")
        let base = Self.makeLiveState()
        let baseSnap = WorkerSlotSnapshot.build(
            slot: slot,
            liveState: base,
            liveStatusEnabled: true
        )

        let flips: [(String, WorkerLiveState)] = [
            ("name", Self.makeLiveState(name: "Worf")),
            ("activity", Self.makeLiveState(activity: .idle)),
            ("liveStatus", Self.makeLiveState(liveStatus: "something else")),
            ("recoveryStatus", Self.makeLiveState(recoveryStatus: "recovering from API error")),
            ("lastEventAt", Self.makeLiveState(lastEventAt: "2026-09-01T00:00:00Z")),
            ("waitReason", Self.makeLiveState(waitReason: "CI build")),
            ("waitExpiresAt", Self.makeLiveState(waitExpiresAt: "2026-09-01T00:00:00Z")),
            ("agentType", Self.makeLiveState(agentType: "review")),
            ("reviewStage", Self.makeLiveState(reviewStage: "consolidation")),
            ("projectName", Self.makeLiveState(projectName: "Nine")),
            ("pool", Self.makeLiveState(pool: "automation")),
            ("tmuxHosted", Self.makeLiveState(tmuxHosted: false)),
            ("viewerAttachError", Self.makeLiveState(viewerAttachError: "the app refused the viewer")),
        ]
        for (field, other) in flips {
            let otherSnap = WorkerSlotSnapshot.build(
                slot: slot,
                liveState: other,
                liveStatusEnabled: true
            )
            XCTAssertNotEqual(
                baseSnap,
                otherSnap,
                "rendered live-state field \(field) did not flip snapshot equality"
            )
        }
    }

    /// The agent pane and the Doing card word the consolidating agent
    /// differently from a parallel reviewer, from the engine's stage alone.
    func testConsolidationStageReadsDifferentlyFromReviewersOnPaneAndCard() throws {
        let client = EngineClient(socketPath: "/tmp/stage-test-\(UUID().uuidString).sock")
        func slice(stage: String?) throws -> WorkerSlotLiveSlice {
            var payload: [String: Any] = [
                "slot_id": 1, "run_id": "exec-1", "model": "m", "shell_pid": 1,
                "activity": "working", "agent_type": "review",
            ]
            payload["review_stage"] = stage
            return WorkerSlotLiveSlice(try XCTUnwrap(client.parseWorkerLiveState(payload)))
        }
        XCTAssertEqual(try slice(stage: "reviewers").badgeLabel, "Reviewing")
        XCTAssertEqual(try slice(stage: nil).badgeLabel, "Reviewing")
        XCTAssertEqual(try slice(stage: "consolidation").badgeLabel, "Consolidating")
        // A stage never relabels a non-review run.
        XCTAssertEqual(AgentType.coding.badgeLabel(reviewStage: "consolidation"), "Coding")

        XCTAssertEqual(AgentActivityState.aiReviewCaption(stage: "reviewers"), "AI review in progress")
        XCTAssertEqual(AgentActivityState.aiReviewCaption(stage: "consolidation"), "Consolidating review findings")
        func cardState(stage: String?) -> AgentActivityState {
            .forDoingCard(
                runtime: nil, liveState: nil, isDispatchPending: false,
                isResolvingConflicts: false, isRemediatingCI: false,
                isAIReviewing: true, aiReviewStage: stage)
        }
        XCTAssertEqual(cardState(stage: "reviewers"), .waiting(reason: "AI review in progress"))
        XCTAssertEqual(cardState(stage: "consolidation"), .waiting(reason: "Consolidating review findings"))
        XCTAssertNotEqual(cardState(stage: "reviewers"), cardState(stage: "consolidation"))
    }

    func testSlotIdentityFieldsFlipEquality() {
        let base = WorkerSlot(
            slotId: 1,
            runId: "exec-1",
            summary: "fixing the scraper",
            taskTitle: "kanban cards"
        )
        let live = Self.makeLiveState()
        let baseSnap = WorkerSlotSnapshot.build(
            slot: base,
            liveState: live,
            liveStatusEnabled: true
        )

        XCTAssertNotEqual(
            baseSnap,
            WorkerSlotSnapshot.build(
                slot: WorkerSlot(
                    slotId: 2,
                    runId: base.runId,
                    summary: base.summary,
                    taskTitle: base.taskTitle
                ),
                liveState: live,
                liveStatusEnabled: true
            )
        )
        XCTAssertNotEqual(
            baseSnap,
            WorkerSlotSnapshot.build(
                slot: WorkerSlot(
                    slotId: base.slotId,
                    runId: "exec-2",
                    summary: base.summary,
                    taskTitle: base.taskTitle
                ),
                liveState: live,
                liveStatusEnabled: true
            )
        )
        XCTAssertNotEqual(
            baseSnap,
            WorkerSlotSnapshot.build(
                slot: WorkerSlot(
                    slotId: base.slotId,
                    runId: base.runId,
                    summary: "rewriting the fencer",
                    taskTitle: base.taskTitle
                ),
                liveState: live,
                liveStatusEnabled: true
            )
        )
        XCTAssertNotEqual(
            baseSnap,
            WorkerSlotSnapshot.build(
                slot: WorkerSlot(
                    slotId: base.slotId,
                    runId: base.runId,
                    summary: base.summary,
                    taskTitle: "other title"
                ),
                liveState: live,
                liveStatusEnabled: true
            )
        )
        var attached = base
        attached.session = Self.makeSession(id: "run-exec-1")
        XCTAssertNotEqual(baseSnap, WorkerSlotSnapshot.build(slot: attached, liveState: live, liveStatusEnabled: true))

    }

    func testLiveStatusEnabledFlipsEquality() {
        let slot = WorkerSlot(slotId: 1)
        let a = WorkerSlotSnapshot.build(slot: slot, liveState: nil, liveStatusEnabled: true)
        let b = WorkerSlotSnapshot.build(slot: slot, liveState: nil, liveStatusEnabled: false)
        XCTAssertNotEqual(a, b)
    }

    func testNilVsPresentLiveStateFlipsEquality() {
        let slot = WorkerSlot(slotId: 1, runId: "exec-1")
        let with = WorkerSlotSnapshot.build(
            slot: slot,
            liveState: Self.makeLiveState(),
            liveStatusEnabled: true
        )
        let without = WorkerSlotSnapshot.build(
            slot: slot,
            liveState: nil,
            liveStatusEnabled: true
        )
        XCTAssertNotEqual(with, without)
    }

    func testSessionIdentityParticipatesInEquality() {
        let sessionA = Self.makeSession(id: "run-a")
        let sessionB = Self.makeSession(id: "run-b")
        var slotA = WorkerSlot(slotId: 1, runId: "a")
        slotA.session = sessionA
        var slotB = WorkerSlot(slotId: 1, runId: "a")
        slotB.session = sessionB
        var slotAAgain = WorkerSlot(slotId: 1, runId: "a")
        slotAAgain.session = sessionA

        let snapA = WorkerSlotSnapshot.build(slot: slotA, liveState: nil, liveStatusEnabled: true)
        let snapB = WorkerSlotSnapshot.build(slot: slotB, liveState: nil, liveStatusEnabled: true)
        let snapA2 = WorkerSlotSnapshot.build(slot: slotAAgain, liveState: nil, liveStatusEnabled: true)
        XCTAssertEqual(snapA, snapA2)
        XCTAssertNotEqual(snapA, snapB)
    }

    // MARK: - WorkerSlotView Equatable

    func testSlotViewEquatableIgnoresClosures() {
        let snap = WorkerSlotSnapshot.build(
            slot: WorkerSlot(slotId: 1),
            liveState: nil,
            liveStatusEnabled: true
        )
        let runtime = GhosttyRuntime.shared
        let a = WorkerSlotView(runtime: runtime, snapshot: snap, onToggleLiveStatus: { _ in })
        let b = WorkerSlotView(runtime: runtime, snapshot: snap, onToggleLiveStatus: { _ in })
        XCTAssertEqual(a, b)
    }

    func testSlotViewEquatableTracksSnapshot() {
        let runtime = GhosttyRuntime.shared
        let a = WorkerSlotView(
            runtime: runtime,
            snapshot: WorkerSlotSnapshot.build(
                slot: WorkerSlot(slotId: 1),
                liveState: nil,
                liveStatusEnabled: true
            ),
            onToggleLiveStatus: { _ in }
        )
        let b = WorkerSlotView(
            runtime: runtime,
            snapshot: WorkerSlotSnapshot.build(
                slot: WorkerSlot(slotId: 2),
                liveState: nil,
                liveStatusEnabled: true
            ),
            onToggleLiveStatus: { _ in }
        )
        XCTAssertNotEqual(a, b)
    }

    func testExitedMemberDoesNotConnect() {
        let snapshot = WorkerSlotSnapshot.build(
            slot: WorkerSlot(slotId: 1, runId: "exec-1"),
            liveState: Self.makeLiveState(activity: .terminated, tmuxHosted: true),
            liveStatusEnabled: true
        )
        XCTAssertTrue(snapshot.isViewerMissing)
        XCTAssertFalse(snapshot.isViewerConnecting)
        XCTAssertEqual(snapshot.viewerMissingMessage, "Worker exited")
    }

    // MARK: - Viewer-missing card

    /// A live member with no viewer and no known failure is still
    /// connecting; the card must say so rather than "not attached".
    func testViewerMissingCardReadsConnectingWhileNothingHasFailed() {
        let slot = WorkerSlot(slotId: 1, runId: "exec-1")
        let connecting = WorkerSlotSnapshot.build(
            slot: slot, liveState: Self.makeLiveState(tmuxHosted: true), liveStatusEnabled: true
        )
        XCTAssertTrue(connecting.isViewerMissing)
        XCTAssertTrue(connecting.isViewerConnecting)
        XCTAssertEqual(connecting.viewerMissingMessage, "Connecting…")

        // Before any live state has arrived the card is equally honest.
        let preLive = WorkerSlotSnapshot.build(slot: slot, liveState: nil, liveStatusEnabled: true)
        XCTAssertTrue(preLive.isViewerConnecting)
        XCTAssertEqual(preLive.viewerMissingMessage, "Connecting…")

        // A placeholder (released run) is not connecting to anything.
        let placeholder = WorkerSlotSnapshot.build(
            slot: slot, liveState: Self.makeLiveState(tmuxHosted: true), liveStatusEnabled: true, isPlaceholder: true
        )
        XCTAssertFalse(placeholder.isViewerConnecting)

        // Once a viewer exists there is nothing to connect.
        let attached = WorkerSlotSnapshot.build(
            slot: WorkerSlot(
                slotId: 1,
                session: TerminalPaneSession(
                    id: "run-exec-1", role: .worker(slot: 1),
                    launchSpec: TerminalLaunchSpec(fontSize: 10, workingDirectory: "/", initialInput: "")
                ),
                runId: "exec-1"
            ),
            liveState: Self.makeLiveState(tmuxHosted: true), liveStatusEnabled: true
        )
        XCTAssertFalse(attached.isViewerMissing)
        XCTAssertFalse(attached.isViewerConnecting)
    }

    /// Failure reasons win over "Connecting…", most specific first: the
    /// app's own refusal, then the engine-reported attach error, then a
    /// worker the engine can never attach (no durable tmux identity).
    func testViewerMissingCardShowsTheFailureReasonInsteadOfConnecting() {
        let slot = WorkerSlot(slotId: 1, runId: "exec-1")

        let engineFailed = WorkerSlotSnapshot.build(
            slot: slot,
            liveState: Self.makeLiveState(tmuxHosted: true, viewerAttachError: "request to the app failed: timed out"),
            liveStatusEnabled: true
        )
        XCTAssertFalse(engineFailed.isViewerConnecting)
        XCTAssertEqual(
            engineFailed.viewerMissingMessage,
            "Couldn't attach the terminal: request to the app failed: timed out"
        )

        let appRefused = WorkerSlotSnapshot.build(
            slot: slot,
            liveState: Self.makeLiveState(tmuxHosted: true, viewerAttachError: "request to the app failed: timed out"),
            liveStatusEnabled: true,
            viewerAttachFailure: "its slot is still hosting run exec-0"
        )
        XCTAssertFalse(appRefused.isViewerConnecting)
        XCTAssertEqual(
            appRefused.viewerMissingMessage,
            "Couldn't attach the terminal: its slot is still hosting run exec-0",
            "the app's own refusal is the more specific half of the same failed exchange"
        )

        let noIdentity = WorkerSlotSnapshot.build(
            slot: slot, liveState: Self.makeLiveState(tmuxHosted: false), liveStatusEnabled: true
        )
        XCTAssertFalse(noIdentity.isViewerConnecting)
        XCTAssertEqual(noIdentity.viewerMissingMessage, AgentActivityState.identityUnavailableReason)
    }

    func testViewerAttachFailureFlipsEquality() {
        let slot = WorkerSlot(slotId: 1, runId: "exec-1")
        let live = Self.makeLiveState(tmuxHosted: true)
        let plain = WorkerSlotSnapshot.build(slot: slot, liveState: live, liveStatusEnabled: true)
        let failed = WorkerSlotSnapshot.build(
            slot: slot, liveState: live, liveStatusEnabled: true, viewerAttachFailure: "its slot is busy"
        )
        XCTAssertNotEqual(plain, failed)
        XCTAssertEqual(
            failed,
            WorkerSlotSnapshot.build(slot: slot, liveState: live, liveStatusEnabled: true, viewerAttachFailure: "its slot is busy")
        )
    }

    func testParseWorkerLiveStateReadsViewerAttachError() throws {
        let client = EngineClient(socketPath: "/tmp/viewer-error-test-\(UUID().uuidString).sock")
        let failed = try XCTUnwrap(client.parseWorkerLiveState([
            "slot_id": 3, "run_id": "run-v", "model": "opus", "activity": "working",
            "tmux_hosted": true, "viewer_attach_error": "the app refused the viewer: unknown slot",
        ]))
        XCTAssertEqual(failed.viewerAttachError, "the app refused the viewer: unknown slot")
        let clean = try XCTUnwrap(client.parseWorkerLiveState([
            "slot_id": 3, "run_id": "run-v", "model": "opus", "activity": "working",
        ]))
        XCTAssertNil(clean.viewerAttachError)
        XCTAssertNotEqual(failed, clean, "the attach error participates in live-state equality so a change republishes")
    }

    func testPlaceholderAndViewerMissingFlipEquality() {
        let slot = WorkerSlot(slotId: 1, runId: "exec-1")
        let live = Self.makeLiveState()
        let occupied = WorkerSlotSnapshot.build(slot: slot, liveState: live, liveStatusEnabled: true)
        let placeholder = WorkerSlotSnapshot.build(
            slot: slot, liveState: live, liveStatusEnabled: true, isPlaceholder: true
        )
        let named = WorkerSlotSnapshot.build(
            slot: slot, liveState: live, liveStatusEnabled: true, cachedPersona: "Riker"
        )
        XCTAssertTrue(occupied.isViewerMissing)
        XCTAssertFalse(placeholder.isViewerMissing)
        XCTAssertNotEqual(occupied, placeholder)
        XCTAssertNotEqual(occupied, named)
        XCTAssertEqual(named.displayName, "Riker")
    }

    /// The grid's render rule: a run the engine no longer lists is finished,
    /// with or without its viewer, and never the viewer-missing card with an
    /// "Unknown" type or an "is working" header. "Unknown" is reserved for a
    /// listed run whose wire type is missing or unrecognised.
    func testReleasedRunNeverRendersUnknown() {
        let session = TerminalPaneSession(
            id: "run-exec-1", role: .worker(slot: 1),
            launchSpec: TerminalLaunchSpec(fontSize: 10, workingDirectory: "/tmp", initialInput: "")
        )
        let attached = WorkerSlot(slotId: 1, session: session, runId: "exec-1", summary: "fixing the scraper", taskTitle: "scraper")

        let detachPending = WorkerSlotSnapshot.render(
            runId: "exec-1", viewer: attached, liveState: nil, liveStatusDisabledSlotIDs: [], cachedPersona: "Riker"
        )
        XCTAssertTrue(detachPending.isPlaceholder)
        XCTAssertEqual(detachPending.bodyKind, .placeholder)
        XCTAssertNotNil(detachPending.session)
        XCTAssertTrue(detachPending.session === session)
        XCTAssertEqual(detachPending.headerTitle, "Riker finished")
        XCTAssertNil(detachPending.headerBadge)
        XCTAssertEqual(detachPending.statusPillLabel, "Finished")
        XCTAssertFalse(detachPending.showsLiveStatusToggle)

        let detached = WorkerSlotSnapshot.render(
            runId: "exec-1", viewer: nil, liveState: nil, liveStatusDisabledSlotIDs: [], cachedPersona: "Riker"
        )
        XCTAssertEqual(detached.bodyKind, .placeholder)
        XCTAssertFalse(detached.isViewerMissing)
        XCTAssertEqual(detached.headerTitle, "Riker finished")
        XCTAssertNil(detached.headerBadge)
        XCTAssertEqual(detached.statusPillLabel, "Finished")
        XCTAssertEqual(detached.slotId, 0)

        // Persona cache already gone: still finished, never "is working".
        let anonymous = WorkerSlotSnapshot.render(
            runId: "exec-1", viewer: nil, liveState: nil, liveStatusDisabledSlotIDs: [], cachedPersona: nil
        )
        XCTAssertEqual(anonymous.headerTitle, "Worker exec-1 finished")
        XCTAssertNil(anonymous.headerBadge)

        // A listed run without a viewer is the viewer-missing card with its
        // real type and activity; a missing wire type is the one Unknown.
        let listed = WorkerSlotSnapshot.render(
            runId: "exec-1", viewer: nil, liveState: Self.makeLiveState(slotId: 7, agentType: "coding", pool: "main"),
            liveStatusDisabledSlotIDs: [7], cachedPersona: "Riker"
        )
        XCTAssertFalse(listed.isPlaceholder)
        XCTAssertEqual(listed.bodyKind, .viewerMissing)
        XCTAssertTrue(listed.isViewerMissing)
        XCTAssertEqual(listed.slotId, 7)
        XCTAssertFalse(listed.liveStatusEnabled)
        XCTAssertEqual(listed.headerBadge, .coding)
        XCTAssertEqual(listed.statusPillLabel, "Working")
        XCTAssertEqual(listed.headerTitle, "Riker is working")
        let untyped = WorkerSlotSnapshot.render(
            runId: "exec-1", viewer: attached, liveState: Self.makeLiveState(agentType: nil),
            liveStatusDisabledSlotIDs: [], cachedPersona: nil
        )
        XCTAssertEqual(untyped.headerBadge, .unknown)
        XCTAssertEqual(untyped.headerTitle, "Worker exec-1 is fixing the scraper")
        XCTAssertTrue(untyped.showsLiveStatusToggle)
    }

    func testIdleReplacesKindBadgeUntilWorkResumes() {
        for kind in ["coding", "design", "review", "automation", "answer"] {
            for activity in [WorkerActivity.working, .idle, .working] {
                let snapshot = WorkerSlotSnapshot.render(
                    runId: "exec-1", viewer: nil,
                    liveState: Self.makeLiveState(activity: activity, agentType: kind),
                    liveStatusDisabledSlotIDs: [], cachedPersona: nil
                )
                if activity == .idle {
                    XCTAssertNil(snapshot.headerBadge, kind)
                    XCTAssertEqual(snapshot.statusPillLabel, "Idle")
                } else {
                    XCTAssertNotNil(snapshot.headerBadge, kind)
                }
            }
        }
    }

    func testHeaderTitleFallsBackFromSummaryToTaskTitle() {
        let live = Self.makeLiveState(name: "Crusher")
        let titled = WorkerSlotSnapshot.build(
            slot: WorkerSlot(slotId: 1, runId: "exec-1", summary: nil, taskTitle: "kanban: revision cards"),
            liveState: live, liveStatusEnabled: true
        )
        XCTAssertEqual(titled.headerTitle, "Crusher: kanban: revision cards")
        let bare = WorkerSlotSnapshot.build(slot: WorkerSlot(slotId: 1, runId: "exec-1"), liveState: live, liveStatusEnabled: true)
        XCTAssertEqual(bare.headerTitle, "Crusher is working")
        XCTAssertNil(WorkerSlotSnapshot.build(slot: WorkerSlot(slotId: 1), liveState: nil, liveStatusEnabled: true).statusPillLabel)
    }

    func testParseMembershipMetadata() throws {
        let client = EngineClient(socketPath: "/tmp/meta-test-\(UUID().uuidString).sock")
        let payload: [String: Any] = [
            "slot_id": 3, "run_id": "run-meta", "model": "opus", "activity": "spawning",
            "name": "Riker", "agent_type": "coding", "project_id": "proj-1",
            "project_name": "Nine", "host_id": "local", "started_at": "2026-10-09T00:00:00Z",
            "pool": "main",
        ]
        let live = try XCTUnwrap(client.parseWorkerLiveState(payload))
        XCTAssertEqual(live.agentType, "coding")
        XCTAssertEqual(live.projectId, "proj-1")
        XCTAssertEqual(live.projectName, "Nine")
        XCTAssertEqual(live.hostId, WorkerLiveState.localHostId)
        XCTAssertEqual(live.startedAt, "2026-10-09T00:00:00Z")
        XCTAssertEqual(live.pool, "main")
        XCTAssertEqual(live.badgeType, .coding)
        XCTAssertEqual(live.agentProject, .project(id: "proj-1"))
        XCTAssertTrue(live.isLocalMember)
        XCTAssertEqual(live.activity.agentsPaneLabel, "Starting")
    }

    // MARK: - WorkersDetailView Equatable

    /// Closures must not participate in `==` — otherwise every ContentView
    /// re-render that rebuilds the toggle handler would defeat `.equatable()`
    /// and re-lay-out all 40 slots.
    func testDetailViewEquatableIgnoresClosures() {
        let workspace = WorkersWorkspaceModel()
        let liveStates = LiveWorkerStateStore()
        let a = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            isVisible: true,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: { _, _ in }
        )
        let b = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            isVisible: true,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: { _, _ in }
        )
        XCTAssertEqual(a, b)
    }

    func testDetailViewEquatableTracksLiveStatusDisabledSet() {
        let workspace = WorkersWorkspaceModel()
        let liveStates = LiveWorkerStateStore()
        let on: (Int, Bool) -> Void = { _, _ in }
        let a = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            isVisible: true,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: on
        )
        let b = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            isVisible: true,
            liveStatusDisabledSlotIDs: [3],
            onToggleLiveStatus: on
        )
        XCTAssertNotEqual(a, b)
    }

    func testDetailViewEquatableTracksVisibility() {
        let workspace = WorkersWorkspaceModel()
        let liveStates = LiveWorkerStateStore()
        let on: (Int, Bool) -> Void = { _, _ in }
        let visible = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            isVisible: true,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: on
        )
        let hidden = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            isVisible: false,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: on
        )
        XCTAssertNotEqual(visible, hidden)
    }

    func testDetailViewEquatableIgnoresUnrelatedStoreIdentityStability() {
        // Same workspace + live-state objects → equal, which is what lets
        // ContentView skip Agents body on an unrelated ChatViewModel publish.
        let workspace = WorkersWorkspaceModel()
        let liveStates = LiveWorkerStateStore()
        let a = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            isVisible: true,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: { _, _ in }
        )
        let otherWorkspace = WorkersWorkspaceModel()
        let c = WorkersDetailView(
            workspace: otherWorkspace,
            liveStates: liveStates,
            isVisible: true,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: { _, _ in }
        )
        XCTAssertNotEqual(a, c)
    }

    // MARK: - Helpers

    private static func makeSession(id: String) -> TerminalPaneSession {
        TerminalPaneSession(
            id: id,
            role: .worker(slot: 1),
            launchSpec: TerminalLaunchSpec(
                fontSize: 12,
                workingDirectory: "/tmp",
                initialInput: ""
            )
        )
    }

    private static func makeLiveState(
        name: String? = nil,
        slotId: Int = 1,
        runId: String = "exec-1",
        model: String = "m",
        shellPid: Int32 = 1,
        lastEventAt: String? = "t",
        currentTool: String? = nil,
        lastToolEndedAt: String? = nil,
        activity: WorkerActivity = .working,
        liveStatus: String? = "Building",
        liveStatusAt: String? = "t",
        recoveryStatus: String? = nil,
        tmuxHosted: Bool? = nil,
        waitReason: String? = nil,
        waitExpiresAt: String? = nil,
        agentType: String? = nil,
        reviewStage: String? = nil,
        projectId: String? = nil,
        projectName: String? = nil,
        hostId: String? = nil,
        startedAt: String? = nil,
        pool: String? = nil,
        viewerAttachError: String? = nil
    ) -> WorkerLiveState {
        WorkerLiveState(
            name: name,
            slotId: slotId,
            runId: runId,
            model: model,
            shellPid: shellPid,
            lastEventAt: lastEventAt,
            currentTool: currentTool,
            lastToolEndedAt: lastToolEndedAt,
            activity: activity,
            liveStatus: liveStatus,
            liveStatusAt: liveStatusAt,
            recoveryStatus: recoveryStatus,
            tmuxHosted: tmuxHosted,
            waitReason: waitReason,
            waitExpiresAt: waitExpiresAt,
            agentType: agentType,
            reviewStage: reviewStage,
            projectId: projectId,
            projectName: projectName,
            hostId: hostId,
            startedAt: startedAt,
            pool: pool,
            viewerAttachError: viewerAttachError
        )
    }
}
