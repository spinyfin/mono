import XCTest
@testable import Boss

/// Unit tests for `WorkerSlotSnapshot` and `WorkerSlotView` / `WorkersDetailView`
/// Equatable surfaces. Same gate shape as `WorkCardSnapshotTests`:
/// unused `WorkerLiveState` fields preserve snapshot equality; every
/// rendered field flips it; view `==` tracks the snapshot and ignores
/// closures so `.equatable()` can skip unchanged slot bodies.
@MainActor
final class WorkerSlotSnapshotTests: XCTestCase {

    // MARK: - Exhaustive WorkerLiveState stored-property classification

    /// Fields `WorkerSlotLiveSlice` reads. Each must have a case in
    /// `testEveryRenderedLiveStateFieldFlipsSnapshotEquality`.
    private static let renderedLiveStateFieldNames: Set<String> = [
        "activity",
        "liveStatus",
        "recoveryStatus",
        "lastEventAt",
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
        "tmuxHosted",
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
        let slot = WorkerSlot(slotId: 1, idleFlavorCycle: 3)
        let live = Self.makeLiveState()
        let a = WorkerSlotSnapshot.build(slot: slot, liveState: live, liveStatusEnabled: true)
        let b = WorkerSlotSnapshot.build(slot: slot, liveState: live, liveStatusEnabled: true)
        XCTAssertEqual(a, b)
    }

    func testNonRenderedLiveStateFieldsPreserveEquality() {
        let slot = WorkerSlot(slotId: 1, runId: "exec-1", idleFlavorCycle: 0)
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
            Self.makeLiveState(tmuxHosted: true),
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

    func testRenderedLiveStateFieldsFlipEquality() {
        let slot = WorkerSlot(slotId: 1, runId: "exec-1", idleFlavorCycle: 0)
        let base = Self.makeLiveState()
        let baseSnap = WorkerSlotSnapshot.build(
            slot: slot,
            liveState: base,
            liveStatusEnabled: true
        )

        let flips: [(String, WorkerLiveState)] = [
            ("activity", Self.makeLiveState(activity: .idle)),
            ("liveStatus", Self.makeLiveState(liveStatus: "something else")),
            ("recoveryStatus", Self.makeLiveState(recoveryStatus: "recovering from API error")),
            ("lastEventAt", Self.makeLiveState(lastEventAt: "2026-09-01T00:00:00Z")),
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

    func testSlotIdentityFieldsFlipEquality() {
        let base = WorkerSlot(
            slotId: 1,
            runId: "exec-1",
            summary: "fixing the scraper",
            taskTitle: "kanban cards",
            idleFlavorCycle: 3
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
                    taskTitle: base.taskTitle,
                    idleFlavorCycle: base.idleFlavorCycle
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
                    taskTitle: base.taskTitle,
                    idleFlavorCycle: base.idleFlavorCycle
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
                    taskTitle: base.taskTitle,
                    idleFlavorCycle: base.idleFlavorCycle
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
                    taskTitle: "other title",
                    idleFlavorCycle: base.idleFlavorCycle
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
                    taskTitle: base.taskTitle,
                    idleFlavorCycle: 99
                ),
                liveState: live,
                liveStatusEnabled: true
            )
        )
    }

    func testLiveStatusEnabledFlipsEquality() {
        let slot = WorkerSlot(slotId: 1, idleFlavorCycle: 0)
        let a = WorkerSlotSnapshot.build(slot: slot, liveState: nil, liveStatusEnabled: true)
        let b = WorkerSlotSnapshot.build(slot: slot, liveState: nil, liveStatusEnabled: false)
        XCTAssertNotEqual(a, b)
    }

    func testNilVsPresentLiveStateFlipsEquality() {
        let slot = WorkerSlot(slotId: 1, runId: "exec-1", idleFlavorCycle: 0)
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
        var slotA = WorkerSlot(slotId: 1, runId: "a", idleFlavorCycle: 0)
        slotA.session = sessionA
        var slotB = WorkerSlot(slotId: 1, runId: "a", idleFlavorCycle: 0)
        slotB.session = sessionB
        var slotAAgain = WorkerSlot(slotId: 1, runId: "a", idleFlavorCycle: 0)
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
            slot: WorkerSlot(slotId: 1, idleFlavorCycle: 0),
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
                slot: WorkerSlot(slotId: 1, idleFlavorCycle: 0),
                liveState: nil,
                liveStatusEnabled: true
            ),
            onToggleLiveStatus: { _ in }
        )
        let b = WorkerSlotView(
            runtime: runtime,
            snapshot: WorkerSlotSnapshot.build(
                slot: WorkerSlot(slotId: 1, idleFlavorCycle: 1),
                liveState: nil,
                liveStatusEnabled: true
            ),
            onToggleLiveStatus: { _ in }
        )
        XCTAssertNotEqual(a, b)
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
            tmuxHostingEnabled: true,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: { _, _ in }
        )
        let b = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            tmuxHostingEnabled: true,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: { _, _ in }
        )
        XCTAssertEqual(a, b)
    }

    func testDetailViewEquatableTracksTmuxHostingFlag() {
        let workspace = WorkersWorkspaceModel()
        let liveStates = LiveWorkerStateStore()
        let on: (Int, Bool) -> Void = { _, _ in }
        let a = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            tmuxHostingEnabled: true,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: on
        )
        let b = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            tmuxHostingEnabled: false,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: on
        )
        XCTAssertNotEqual(a, b)
    }

    func testDetailViewEquatableTracksLiveStatusDisabledSet() {
        let workspace = WorkersWorkspaceModel()
        let liveStates = LiveWorkerStateStore()
        let on: (Int, Bool) -> Void = { _, _ in }
        let a = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            tmuxHostingEnabled: true,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: on
        )
        let b = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            tmuxHostingEnabled: true,
            liveStatusDisabledSlotIDs: [3],
            onToggleLiveStatus: on
        )
        XCTAssertNotEqual(a, b)
    }

    func testDetailViewEquatableIgnoresUnrelatedStoreIdentityStability() {
        // Same workspace + live-state objects → equal, which is what lets
        // ContentView skip Agents body on an unrelated ChatViewModel publish.
        let workspace = WorkersWorkspaceModel()
        let liveStates = LiveWorkerStateStore()
        let a = WorkersDetailView(
            workspace: workspace,
            liveStates: liveStates,
            tmuxHostingEnabled: true,
            liveStatusDisabledSlotIDs: [],
            onToggleLiveStatus: { _, _ in }
        )
        let otherWorkspace = WorkersWorkspaceModel()
        let c = WorkersDetailView(
            workspace: otherWorkspace,
            liveStates: liveStates,
            tmuxHostingEnabled: true,
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
        tmuxHosted: Bool? = nil
    ) -> WorkerLiveState {
        WorkerLiveState(
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
            tmuxHosted: tmuxHosted
        )
    }
}
