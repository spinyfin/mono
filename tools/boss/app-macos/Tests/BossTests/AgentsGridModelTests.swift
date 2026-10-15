import AppKit
import PaneLayout
import XCTest
@testable import Boss

@MainActor
final class AgentsGridModelTests: XCTestCase {
    func testRemoteWorkersAreExcludedFromMembership() {
        let grid = AgentsGridModel(isVisible: true)
        grid.apply(
            states: [
                makeState(runId: "local-1", hostId: WorkerLiveState.localHostId),
                makeState(runId: "remote-1", slotId: 200, hostId: "ssh-host"),
            ],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.visibleCount, 1)
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "local-1"), 0)
        XCTAssertNil(grid.layout.logicalCellIndex(ofRun: "remote-1"))
    }

    func testMissingHostIdDoesNotInventLocalMembership() {
        let grid = AgentsGridModel()
        grid.apply(
            states: [makeState(runId: "legacy", hostId: nil)],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertTrue(grid.layout.isEmpty)
    }

    func testAttachBeforeSnapshotDoesNotInventMembership() {
        let workspace = WorkersWorkspaceModel()
        _ = workspace.attachWorkerPane(makeAttach(runId: "early", slot: 1))
        let grid = AgentsGridModel()
        grid.apply(
            states: [],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertTrue(grid.layout.isEmpty)
        XCTAssertEqual(grid.unplacedViewers.map(\.runId), ["early"])
        XCTAssertNotNil(workspace.viewersByRunId["early"]?.session)
    }

    func testSnapshotThenAttachFillsTheSameCell() throws {
        let workspace = WorkersWorkspaceModel()
        let grid = AgentsGridModel()
        let state = makeState(runId: "run-a")
        grid.apply(
            states: [state],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.visibleCount, 1)
        XCTAssertTrue(grid.unplacedViewers.isEmpty)

        _ = workspace.attachWorkerPane(makeAttach(runId: "run-a", slot: 1))
        grid.apply(
            states: [state],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "run-a"), 0)
        let page = try XCTUnwrap(grid.layout.currentPage)
        guard case .occupied(let member) = page.cells[0].content else {
            XCTFail("expected occupied cell")
            return
        }
        XCTAssertTrue(member.hasViewer)
        XCTAssertTrue(grid.unplacedViewers.isEmpty)
    }

    func testEngineReleaseThenDelayedDetachDoesNotResurrectMembership() {
        let workspace = WorkersWorkspaceModel()
        _ = workspace.attachWorkerPane(makeAttach(runId: "run-a", slot: 1))
        let grid = AgentsGridModel()
        grid.apply(
            states: [makeState(runId: "run-a")],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.visibleCount, 1)

        grid.apply(
            states: [],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertTrue(grid.layout.isEmpty)
        XCTAssertEqual(grid.unplacedViewers.map(\.runId), ["run-a"])

        _ = workspace.detachWorkerPane(runId: "run-a")
        grid.apply(
            states: [],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertTrue(grid.layout.isEmpty)
        XCTAssertTrue(grid.unplacedViewers.isEmpty)
        XCTAssertTrue(workspace.viewersByRunId.isEmpty)
    }

    func testRejectedStoreSnapshotLeavesGridUnchanged() {
        let store = LiveWorkerStateStore()
        store.update(states: [makeState(runId: "run-a")])
        let grid = AgentsGridModel()
        grid.apply(
            states: Array(store.byRunID.values),
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        store.update(states: [
            makeState(runId: "run-a", slotId: 1),
            makeState(runId: "run-a", slotId: 2),
        ])
        grid.apply(
            states: Array(store.byRunID.values),
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: store.feedUnavailable
        )
        XCTAssertEqual(store.byRunID.count, 1)
        XCTAssertEqual(grid.layout.visibleCount, 1)
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "run-a"), 0)
    }

    func testFeedUnavailableRetainsLastSnapshot() {
        let store = LiveWorkerStateStore()
        store.update(states: [makeState(runId: "run-a", activity: .working)])
        store.markFeedUnavailable()
        let grid = AgentsGridModel()
        grid.apply(
            states: Array(store.byRunID.values),
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: store.feedUnavailable
        )
        XCTAssertTrue(grid.feedUnavailable)
        XCTAssertEqual(grid.layout.visibleCount, 1)

        store.update(states: [makeState(runId: "run-a", activity: .idle)])
        grid.apply(
            states: Array(store.byRunID.values),
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: store.feedUnavailable
        )
        XCTAssertFalse(grid.feedUnavailable)
        XCTAssertEqual(store.byRunID["run-a"]?.activity, .idle)
    }

    func testTerminatedMemberRendersAsExitedUntilRelease() {
        let grid = AgentsGridModel()
        grid.apply(
            states: [makeState(runId: "run-a", activity: .terminated)],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.visibleCount, 1)
        let live = makeState(runId: "run-a", activity: .terminated)
        let snapshot = WorkerSlotSnapshot.build(
            slot: WorkerSlot(slotId: 1, runId: "run-a"),
            liveState: live,
            liveStatusEnabled: true
        )
        XCTAssertTrue(snapshot.isExited)
        XCTAssertEqual(live.activity.agentsPaneLabel, "Exited")
        XCTAssertTrue(snapshot.isViewerMissing)
    }

    func testSpawningUsesStartingLabel() {
        XCTAssertEqual(WorkerActivity.spawning.agentsPaneLabel, "Starting")
        XCTAssertEqual(WorkerActivity.working.agentsPaneLabel, "Working")
    }

    func testUnknownAgentTypeStaysInTheGrid() throws {
        let grid = AgentsGridModel()
        grid.apply(
            states: [makeState(runId: "run-a", agentType: "time-travel")],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.visibleCount, 1)
        let page = try XCTUnwrap(grid.layout.currentPage)
        guard case .occupied(let member) = page.cells[0].content else {
            XCTFail("expected occupied cell")
            return
        }
        XCTAssertEqual(member.type, .unknown)
    }

    func testPoolStripUsesAttributedPoolNotSlotRange() {
        let strip = AgentsGridModel.poolStrip(
            states: [
                makeState(runId: "a", slotId: 9, pool: "automation"),
                makeState(runId: "b", slotId: 1, pool: "main"),
                makeState(runId: "c", slotId: 25, pool: "review"),
            ],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16
        )
        XCTAssertEqual(strip, "Interactive 1/16 · Automation 1/8 · Review 1/16")
    }

    func testLiveResizeDefersGeometryUntilEnd() {
        let grid = AgentsGridModel(
            geometry: PaneGeometry(
                area: PaneSize(width: 800, height: 600),
                cell: PaneSize(width: 6, height: 13)
            )
        )
        let before = grid.layout.capacity
        grid.noteArea(width: 400, height: 300, inLiveResize: true)
        XCTAssertEqual(grid.layout.capacity, before)
        grid.liveResizeDidEnd()
        XCTAssertNotEqual(grid.layout.geometry.area.width, 800)
    }

    func testPersonaSurvivesPlaceholderAfterRelease() {
        let grid = AgentsGridModel()
        grid.apply(
            states: [makeState(runId: "run-a", name: "Riker")],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.personaName(for: "run-a"), "Riker")
        // A second member keeps the page count from collapsing to empty
        // so a release can leave a placeholder instead of compacting away.
        grid.apply(
            states: [
                makeState(runId: "run-a", name: "Riker"),
                makeState(runId: "run-b", slotId: 2, name: "Data", startedAt: "2026-10-09T00:00:01Z"),
            ],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        grid.apply(
            states: [makeState(runId: "run-b", slotId: 2, name: "Data", startedAt: "2026-10-09T00:00:01Z")],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        // Forced collapse when fewer pages suffice removes placeholders.
        // Persona for the surviving run remains.
        XCTAssertEqual(grid.personaName(for: "run-b"), "Data")
    }

    func testAgentTypeFromWire() {
        XCTAssertEqual(AgentType.fromWire("coding"), .coding)
        XCTAssertEqual(AgentType.fromWire("review"), .review)
        XCTAssertEqual(AgentType.fromWire(nil), .unknown)
        XCTAssertEqual(AgentType.fromWire("time-travel"), .unknown)
        XCTAssertEqual(AgentType.unknown.badgeLabel, "Unknown")
    }

    func testQuietTaskAndCellCallbackWaitForResizeBoundary() async throws {
        let grid = AgentsGridModel()
        let before = grid.layout.geometry
        grid.noteArea(width: 1200, height: 900, inLiveResize: false)
        grid.noteArea(width: 1400, height: 1000, inLiveResize: true)
        grid.noteCellSize(width: 6, height: 13)
        try await Task.sleep(nanoseconds: 450_000_000)
        XCTAssertEqual(grid.layout.geometry, before)
        grid.liveResizeDidEnd()
        XCTAssertEqual(grid.layout.geometry.area.width, 1400)
        XCTAssertEqual(grid.layout.geometry.cell.width, 6)
    }

    func testFocusedSeventhRunFollowsCapacityShrink() {
        let grid = AgentsGridModel(
            policy: PaneCapacityPolicy(minColumns: 1, minRows: 1, headerHeight: 0, maxPanesPerPage: 8, gap: 0, deadBand: 0),
            geometry: PaneGeometry(area: PaneSize(width: 400, height: 200), cell: PaneSize(width: 100, height: 100))
        )
        grid.apply(states: (0..<8).map { makeState(runId: "run-\($0)", slotId: $0) },
                   viewers: [:], interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false)
        grid.setFocusedRun("run-6")
        grid.noteArea(width: 200, height: 200, inLiveResize: true)
        grid.liveResizeDidEnd()
        XCTAssertEqual(grid.layout.capacity, 4)
        XCTAssertEqual(grid.layout.selectedPage, 1)
        XCTAssertEqual(grid.layout.focusedRunId, "run-6")
    }

    func testFilterHidesMembersKeepsViewersMountedAndCountsWaiting() {
        let workspace = WorkersWorkspaceModel()
        let grid = AgentsGridModel()
        for (index, run) in ["a", "b", "c"].enumerated() {
            _ = workspace.attachWorkerPane(makeAttach(runId: run, slot: index + 1))
        }
        let sessions = workspace.viewersByRunId.mapValues { $0.session }
        let states = [
            makeState(runId: "a", slotId: 1, agentType: "coding", projectId: "p1", projectName: "Alpha"),
            makeState(runId: "b", slotId: 2, activity: .waitingForInput, agentType: "review", projectId: "p2", projectName: "Beta"),
            makeState(runId: "c", slotId: 3, agentType: "mystery"),
        ]
        grid.apply(states: states, viewers: workspace.viewersByRunId, interactivePoolSize: 16,
                   automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false)
        XCTAssertFalse(grid.layout.filter.isActive)
        XCTAssertEqual(grid.projectOptions, [.project(id: "p1"), .project(id: "p2"), .unfiled])
        XCTAssertEqual(grid.projectLabel(.project(id: "p2")), "Beta")
        XCTAssertTrue(grid.typeOptions.contains(.unknown))

        grid.toggleProject(.project(id: "p1"))
        XCTAssertEqual(grid.layout.visibleCount, 1)
        XCTAssertEqual(grid.layout.hiddenCount, 2)
        XCTAssertEqual(grid.layout.waitingCount, 0)
        XCTAssertEqual(grid.layout.hiddenWaitingCount, 1)
        // Hidden runs keep their mounted viewers; they are just unplaced.
        for (run, session) in sessions { XCTAssertTrue(workspace.viewersByRunId[run]?.session === session) }
        XCTAssertEqual(Set(grid.unplacedViewers.compactMap(\.runId)), ["b", "c"])

        grid.clearFilter()
        grid.toggleProject(.unfiled)
        XCTAssertEqual(grid.layout.visibleCount, 1)
        XCTAssertNotNil(grid.layout.logicalCellIndex(ofRun: "c"))
        grid.clearFilter()
        grid.toggleType(.unknown)
        XCTAssertNotNil(grid.layout.logicalCellIndex(ofRun: "c"))
        XCTAssertEqual(grid.layout.visibleCount, 1)

        grid.clearFilter()
        XCTAssertEqual(grid.layout.visibleCount, 3)
        XCTAssertTrue(grid.unplacedViewers.isEmpty)
    }

    func testFilterStartsAtAllOnEveryNewModel() {
        let grid = AgentsGridModel()
        grid.toggleType(.review)
        XCTAssertTrue(grid.layout.filter.isActive)
        XCTAssertEqual(AgentsGridModel().layout.filter, .all)
    }

    func testAttachedOwnershipSurvivesGrowthTidyCollapseAndGeometry() {
        let workspace = WorkersWorkspaceModel()
        let grid = AgentsGridModel(
            policy: PaneCapacityPolicy(minColumns: 1, minRows: 1, headerHeight: 0, maxPanesPerPage: 8, gap: 0, deadBand: 0),
            geometry: PaneGeometry(area: PaneSize(width: 400, height: 200), cell: PaneSize(width: 100, height: 100))
        )
        for index in 0..<9 { _ = workspace.attachWorkerPane(makeAttach(runId: "run-\(index)", slot: index + 1)) }
        let sessions = workspace.viewersByRunId.mapValues { $0.session }
        func apply(_ count: Int) {
            grid.apply(states: (0..<count).map { makeState(runId: "run-\($0)", slotId: $0 + 1) },
                       viewers: workspace.viewersByRunId, interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false)
            for (run, session) in sessions { XCTAssertTrue(workspace.viewersByRunId[run]?.session === session) }
        }
        apply(0) // Attached before membership.
        XCTAssertEqual(grid.unplacedViewers.count, 9)
        apply(4)
        apply(6) // Shape growth.
        grid.tidy()
        apply(9)
        XCTAssertEqual(grid.layout.pageCount, 2)
        apply(8) // Forced nine-to-eight compaction.
        XCTAssertEqual(grid.layout.pageCount, 1)
        apply(2)
        apply(1) // Retained released viewer.
        grid.tidy()
        grid.selectPage(0)
        grid.noteArea(width: 200, height: 200, inLiveResize: true)
        grid.liveResizeDidEnd()
        XCTAssertEqual(Set(grid.unplacedViewers.compactMap(\.runId)), Set((1..<9).map { "run-\($0)" }))
    }

    // MARK: - Released runs never linger

    /// A release that lands while Agents is hidden applies at once under the
    /// layout's hidden rule, and the released run renders as finished (not a
    /// viewerless live member) until its detach removes it. Re-entering
    /// Agents shows current membership with no placeholder and nothing to
    /// tidy.
    func testReleaseWhileHiddenCompactsAndRendersFinishedUntilDetach() throws {
        let workspace = WorkersWorkspaceModel()
        let grid = roomyGrid()
        let runA = makeState(runId: "run-a", slotId: 1, name: "Riker")
        let runB = makeState(runId: "run-b", slotId: 2, name: "Data", startedAt: "2026-10-09T00:00:01Z")
        _ = workspace.attachWorkerPane(makeAttach(runId: "run-a", slot: 1))
        _ = workspace.attachWorkerPane(makeAttach(runId: "run-b", slot: 2))
        let sessionA = try XCTUnwrap(workspace.viewersByRunId["run-a"]?.session)

        sync(grid, visible: true, states: [runA, runB], viewers: workspace.viewersByRunId)
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "run-a"), 0)
        sync(grid, visible: false, states: [runA, runB], viewers: workspace.viewersByRunId)

        // Hidden: the engine finalizes run-a and drops it from the feed.
        let store = LiveWorkerStateStore()
        store.update(states: [runB])
        sync(grid, visible: false, states: Array(store.byRunID.values), viewers: workspace.viewersByRunId)
        XCTAssertNil(grid.layout.logicalCellIndex(ofRun: "run-a"))
        XCTAssertFalse(grid.layout.pages.contains { page in
            page.cells.contains { if case .placeholder = $0.content { return true } else { return false } }
        }, "hidden releases compact away, no placeholder")
        // The surface stays mounted (detach pending) but is a finished pane.
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-a", "run-b"])
        let pending = render(grid, runId: "run-a", viewer: workspace.viewersByRunId["run-a"], live: store.byRunID["run-a"])
        XCTAssertTrue(pending.isPlaceholder)
        XCTAssertTrue(pending.session === sessionA)
        XCTAssertEqual(pending.bodyKind, .terminal)
        XCTAssertEqual(pending.headerTitle, "Riker finished")
        XCTAssertNil(pending.headerBadge)
        XCTAssertEqual(pending.statusPillLabel, "Finished")

        // Hidden: the detach arrives. The run is no longer rendered at all.
        assertSuccess(workspace.detachWorkerPane(runId: "run-a"))
        sync(grid, visible: false, states: Array(store.byRunID.values), viewers: workspace.viewersByRunId)
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-b"])

        // Back to Agents: current membership, packed, nothing stale.
        sync(grid, visible: true, states: Array(store.byRunID.values), viewers: workspace.viewersByRunId)
        XCTAssertEqual(grid.layout.visibleCount, 1)
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "run-b"), 0)
        XCTAssertFalse(grid.layout.isTidyHighlighted)
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-b"])
        let liveB = render(grid, runId: "run-b", viewer: workspace.viewersByRunId["run-b"], live: store.byRunID["run-b"])
        XCTAssertFalse(liveB.isPlaceholder)
        XCTAssertEqual(liveB.headerBadge, .coding)
        XCTAssertEqual(liveB.headerTitle, "Data is working")
    }

    /// A release while Agents is visible leaves the finished placeholder in
    /// the run's cell. Once the detach removes the viewer it is the finished
    /// card — never the viewer-missing card with an "Unknown" type.
    func testReleaseWhileVisibleRendersFinishedCardAfterDetach() throws {
        let workspace = WorkersWorkspaceModel()
        let grid = roomyGrid()
        let runA = makeState(runId: "run-a", slotId: 1, name: "Riker")
        let runB = makeState(runId: "run-b", slotId: 2, name: "Data", startedAt: "2026-10-09T00:00:01Z")
        _ = workspace.attachWorkerPane(makeAttach(runId: "run-a", slot: 1))
        sync(grid, visible: true, states: [runA, runB], viewers: workspace.viewersByRunId)

        let store = LiveWorkerStateStore()
        store.update(states: [runB])
        sync(grid, visible: true, states: Array(store.byRunID.values), viewers: workspace.viewersByRunId)
        assertSuccess(workspace.detachWorkerPane(runId: "run-a"))
        sync(grid, visible: true, states: Array(store.byRunID.values), viewers: workspace.viewersByRunId)

        let page = try XCTUnwrap(grid.layout.currentPage)
        XCTAssertEqual(page.cells[0].content, .placeholder(runId: "run-a"))
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-a", "run-b"])
        let finished = render(grid, runId: "run-a", viewer: workspace.viewersByRunId["run-a"], live: store.byRunID["run-a"])
        XCTAssertEqual(finished.bodyKind, .placeholder)
        XCTAssertEqual(finished.headerTitle, "Riker finished")
        XCTAssertNil(finished.headerBadge)
        XCTAssertEqual(finished.statusPillLabel, "Finished")
        XCTAssertFalse(finished.showsLiveStatusToggle)
        XCTAssertFalse(finished.isViewerMissing)
    }

    /// Release followed by immediate slot reuse: the new run takes the slot
    /// (after the stale viewer is detached), the old run is never rendered
    /// as live again, and the feed's consecutive slot reuse is accepted by
    /// the store rather than freezing membership.
    func testReleaseThenImmediateSlotReuseShowsOnlyTheNewRun() throws {
        let workspace = WorkersWorkspaceModel()
        let grid = roomyGrid()
        let store = LiveWorkerStateStore()
        let runA = makeState(runId: "run-a", slotId: 1, name: "Riker")
        _ = workspace.attachWorkerPane(makeAttach(runId: "run-a", slot: 1))
        store.update(states: [runA])
        sync(grid, visible: true, states: Array(store.byRunID.values), viewers: workspace.viewersByRunId)

        // Engine frees slot 1 and spawns run-c into it before the old
        // viewer's detach has landed: the attach is refused as SlotBusy,
        // the engine detaches the stale viewer, then retries.
        store.update(states: [])
        sync(grid, visible: true, states: Array(store.byRunID.values), viewers: workspace.viewersByRunId)
        guard case .failure(.slotBusy(let occupant)) = workspace.attachWorkerPane(makeAttach(runId: "run-c", slot: 1)) else {
            XCTFail("expected SlotBusy while run-a's viewer is still attached")
            return
        }
        XCTAssertEqual(occupant, "run-a")
        assertSuccess(workspace.detachWorkerPane(runId: "run-a"))
        guard case .success = workspace.attachWorkerPane(makeAttach(runId: "run-c", slot: 1)) else {
            XCTFail("expected the retried attach to succeed")
            return
        }
        let runC = makeState(runId: "run-c", slotId: 1, name: "Worf", agentType: "review", startedAt: "2026-10-09T00:00:05Z")
        store.update(states: [runC])
        XCTAssertEqual(store.byRunID.keys.sorted(), ["run-c"], "consecutive slot reuse is an accepted snapshot")
        sync(grid, visible: true, states: Array(store.byRunID.values), viewers: workspace.viewersByRunId)

        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-c"])
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "run-c"), 0)
        XCTAssertNil(grid.layout.logicalCellIndex(ofRun: "run-a"))
        let liveC = render(grid, runId: "run-c", viewer: workspace.viewersByRunId["run-c"], live: store.byRunID["run-c"])
        XCTAssertFalse(liveC.isPlaceholder)
        XCTAssertEqual(liveC.slotId, 1)
        XCTAssertEqual(liveC.headerBadge, .review)
        XCTAssertEqual(liveC.headerTitle, "Worf is working")
        XCTAssertEqual(liveC.bodyKind, .terminal)
    }

    /// Same reuse without viewers, while visible: run-a's placeholder cell
    /// is the lowest free cell, so the arrival in its old slot takes that
    /// cell. The released run is then neither placed nor attached and is
    /// not rendered; were it rendered, it would still read as finished.
    func testArrivalReusesReleasedCellAndReleasedRunIsNotRendered() throws {
        let grid = roomyGrid()
        let store = LiveWorkerStateStore()
        let runA = makeState(runId: "run-a", slotId: 1, name: "Riker")
        let runB = makeState(runId: "run-b", slotId: 2, name: "Data", startedAt: "2026-10-09T00:00:01Z")
        store.update(states: [runA, runB])
        sync(grid, visible: true, states: Array(store.byRunID.values), viewers: [:])
        store.update(states: [runB])
        sync(grid, visible: true, states: Array(store.byRunID.values), viewers: [:])
        let runC = makeState(runId: "run-c", slotId: 1, name: "Worf", startedAt: "2026-10-09T00:00:05Z")
        store.update(states: [runB, runC])
        sync(grid, visible: true, states: Array(store.byRunID.values), viewers: [:])

        let page = try XCTUnwrap(grid.layout.currentPage)
        XCTAssertEqual(page.cells[0].content, .occupied(PaneMember(
            runId: "run-c", startedAt: try XCTUnwrap(WorkerStaleness.parse("2026-10-09T00:00:05Z")),
            project: .unfiled, type: .coding, isWaiting: false, hasViewer: false
        )))
        XCTAssertEqual(grid.renderRunIds(attached: [String]()), ["run-b", "run-c"])
        XCTAssertNil(grid.personaName(for: "run-a"))
        let finishedA = render(grid, runId: "run-a", viewer: nil, live: store.byRunID["run-a"])
        XCTAssertTrue(finishedA.isPlaceholder)
        XCTAssertNil(finishedA.headerBadge)
    }

    private func assertSuccess(_ result: EngineReleaseResult, file: StaticString = #filePath, line: UInt = #line) {
        guard case .success = result else {
            XCTFail("expected detach success, got \(result)", file: file, line: line)
            return
        }
    }

    private func sync(
        _ grid: AgentsGridModel,
        visible: Bool,
        states: [WorkerLiveState],
        viewers: [String: WorkerSlot]
    ) {
        grid.apply(
            isVisible: visible,
            states: states,
            viewers: viewers,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
    }

    /// Mirrors `WorkersDetailView.renderSnapshot`: the view's exact inputs.
    private func render(
        _ grid: AgentsGridModel,
        runId: String,
        viewer: WorkerSlot?,
        live: WorkerLiveState?
    ) -> WorkerSlotSnapshot {
        WorkerSlotSnapshot.render(
            runId: runId,
            viewer: viewer,
            liveState: live,
            liveStatusDisabledSlotIDs: [],
            cachedPersona: grid.personaName(for: runId)
        )
    }

    private func roomyGrid() -> AgentsGridModel {
        AgentsGridModel(
            policy: PaneCapacityPolicy(minColumns: 1, minRows: 1, headerHeight: 0, maxPanesPerPage: 8, gap: 0, deadBand: 0),
            geometry: PaneGeometry(area: PaneSize(width: 400, height: 200), cell: PaneSize(width: 100, height: 100))
        )
    }

    func testWorkerCellMeasurementIgnoresBossAndNormalizesBackingScale() {
        let one = BackingScaleView(scale: 1)
        let two = BackingScaleView(scale: 2)
        let points = NSSize(width: 6, height: 13)
        XCTAssertEqual(GhosttyRuntime.workerCellSize(role: .worker(slot: 1), host: one, pixels: points), points)
        XCTAssertEqual(GhosttyRuntime.workerCellSize(role: .worker(slot: 1), host: two, pixels: NSSize(width: 12, height: 26)), points)
        XCTAssertNil(GhosttyRuntime.workerCellSize(role: .boss, host: one, pixels: points))
        XCTAssertNil(GhosttyRuntime.workerCellSize(role: .worker(slot: 1), host: one, pixels: .zero))
    }

    private func makeAttach(runId: String, slot: Int) -> EngineAttachRequest {
        EngineAttachRequest(
            runId: runId,
            slotId: slot,
            sessionName: "boss-\(slot)-\(runId)",
            tmuxSocketPath: "/state/boss/tmux.sock",
            summary: nil,
            taskTitle: nil
        )
    }

    private func makeState(
        runId: String,
        slotId: Int = 1,
        name: String? = nil,
        activity: WorkerActivity = .working,
        agentType: String? = "coding",
        hostId: String? = WorkerLiveState.localHostId,
        startedAt: String = "2026-10-09T00:00:00Z",
        pool: String? = "main",
        projectId: String? = nil,
        projectName: String? = nil
    ) -> WorkerLiveState {
        WorkerLiveState(
            name: name,
            slotId: slotId,
            runId: runId,
            model: "opus",
            shellPid: 1,
            lastEventAt: nil,
            currentTool: nil,
            lastToolEndedAt: nil,
            activity: activity,
            liveStatus: nil,
            liveStatusAt: nil,
            recoveryStatus: nil,
            tmuxHosted: true,
            agentType: agentType,
            projectId: projectId,
            projectName: projectName,
            hostId: hostId,
            startedAt: startedAt,
            pool: pool
        )
    }
}


@MainActor
private final class BackingScaleView: NSView {
    let scale: CGFloat
    init(scale: CGFloat) {
        self.scale = scale
        super.init(frame: .zero)
    }
    required init?(coder: NSCoder) { nil }
    override func convertFromBacking(_ size: NSSize) -> NSSize {
        NSSize(width: size.width / scale, height: size.height / scale)
    }
}
