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
            reviewPoolSize: 16
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
            reviewPoolSize: 16
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
            reviewPoolSize: 16
        )
        XCTAssertTrue(grid.layout.isEmpty)
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["early"])
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
            reviewPoolSize: 16
        )
        XCTAssertEqual(grid.layout.visibleCount, 1)
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-a"])

        _ = workspace.attachWorkerPane(makeAttach(runId: "run-a", slot: 1))
        grid.apply(
            states: [state],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16
        )
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "run-a"), 0)
        let page = try XCTUnwrap(grid.layout.currentPage)
        guard case .occupied(let member) = page.cells[0].content else {
            XCTFail("expected occupied cell")
            return
        }
        XCTAssertTrue(member.hasViewer)
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-a"])
        XCTAssertTrue(grid.placement(for: "run-a", area: CGSize(width: 800, height: 600)).visible)
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
            reviewPoolSize: 16
        )
        XCTAssertEqual(grid.layout.visibleCount, 1)

        grid.apply(
            states: [],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16
        )
        XCTAssertTrue(grid.layout.isEmpty)
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-a"])
        XCTAssertFalse(grid.placement(for: "run-a", area: CGSize(width: 800, height: 600)).visible)

        _ = workspace.detachWorkerPane(runId: "run-a")
        grid.apply(
            states: [],
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16
        )
        XCTAssertTrue(grid.layout.isEmpty)
        XCTAssertTrue(grid.renderRunIds(attached: workspace.viewersByRunId.keys).isEmpty)
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
            reviewPoolSize: 16
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
            reviewPoolSize: 16
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
            reviewPoolSize: 16
        )
        XCTAssertTrue(store.feedUnavailable)
        XCTAssertEqual(grid.layout.visibleCount, 1)

        store.update(states: [makeState(runId: "run-a", activity: .idle)])
        grid.apply(
            states: Array(store.byRunID.values),
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16
        )
        XCTAssertFalse(store.feedUnavailable)
        XCTAssertEqual(store.byRunID["run-a"]?.activity, .idle)
    }

    func testTerminatedMemberRendersAsExitedUntilRelease() {
        let grid = AgentsGridModel()
        grid.apply(
            states: [makeState(runId: "run-a", activity: .terminated)],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16
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
            reviewPoolSize: 16
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

    func testPersonaSurvivesPlaceholderAfterRelease() throws {
        let grid = roomyGrid()
        let runA = makeState(runId: "run-a", name: "Riker")
        let runB = makeState(runId: "run-b", slotId: 2, name: "Data", startedAt: "2026-10-09T00:00:01Z")
        grid.apply(states: [runA, runB], viewers: [:], interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16)
        XCTAssertEqual(grid.personaName(for: "run-a"), "Riker")
        // Capacity is 8, so releasing run-a while visible leaves a placeholder
        // rather than compacting its cell away.
        grid.apply(states: [runB], viewers: [:], interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16)
        let page = try XCTUnwrap(grid.layout.currentPage)
        XCTAssertTrue(page.cells.contains { cell in
            if case .placeholder(let run) = cell.content { return run == "run-a" }
            return false
        })
        XCTAssertEqual(grid.personaName(for: "run-a"), "Riker")
        XCTAssertEqual(grid.personaName(for: "run-b"), "Data")
    }

    func testReturningToAgentsRepacksCurrentMembership() throws {
        let grid = roomyGrid()
        let states = (0..<3).map {
            makeState(runId: "run-\($0)", slotId: $0 + 1, startedAt: "2026-10-09T00:00:0\($0)Z")
        }
        func sync(_ visible: Bool, _ current: [WorkerLiveState]) {
            grid.apply(isVisible: visible, states: current, viewers: [:],
                       interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16)
        }
        sync(true, states)
        sync(false, states)
        // While hidden: run-1 ends and run-3 arrives.
        let arrival = makeState(runId: "run-3", slotId: 4, startedAt: "2026-10-09T00:00:03Z")
        sync(false, [states[0], states[2], arrival])
        sync(true, [states[0], states[2], arrival])
        let page = try XCTUnwrap(grid.layout.currentPage)
        var order: [String] = []
        for cell in page.cells {
            switch cell.content {
            case .occupied(let member): order.append(member.runId)
            case .placeholder(let run): XCTFail("unexpected placeholder for \(run)")
            case .empty: break
            }
        }
        XCTAssertEqual(order, ["run-0", "run-2", "run-3"])
        XCTAssertFalse(grid.layout.isTidyHighlighted)
    }

    func testPlacementUsesPolicyGap() {
        let grid = AgentsGridModel(
            policy: PaneCapacityPolicy(minColumns: 1, minRows: 1, headerHeight: 0, maxPanesPerPage: 8, gap: 10, deadBand: 0),
            geometry: PaneGeometry(area: PaneSize(width: 410, height: 100), cell: PaneSize(width: 100, height: 100))
        )
        grid.apply(
            states: [makeState(runId: "a"), makeState(runId: "b", slotId: 2, startedAt: "2026-10-09T00:00:01Z")],
            viewers: [:], interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16
        )
        let area = CGSize(width: 410, height: 100)
        let first = grid.placement(for: "a", area: area).frame
        let second = grid.placement(for: "b", area: area).frame
        XCTAssertEqual(second.minX - first.maxX, 10, accuracy: 0.001)
    }

    func testFallbackCellKeepsCapacityAboveOneBeforeMeasurement() {
        let grid = AgentsGridModel(
            policy: PaneCapacityPolicy(minColumns: 10, minRows: 10, headerHeight: 0, maxPanesPerPage: 8, gap: 0, deadBand: 0),
            geometry: PaneGeometry(area: PaneSize(width: 800, height: 600), cell: AgentsGridModel.fallbackCell)
        )
        XCTAssertGreaterThan(grid.layout.capacity, 1)
    }

    func testDefaultInitializerSeedsFallbackAndMeasurementReplacesIt() {
        let grid = AgentsGridModel()
        XCTAssertEqual(grid.layout.geometry.cell, AgentsGridModel.fallbackCell)
        grid.noteArea(width: 1600, height: 1000, inLiveResize: true)
        grid.liveResizeDidEnd()
        XCTAssertEqual(grid.layout.geometry.area.width, 1600)
        XCTAssertGreaterThan(grid.layout.capacity, 1)
        grid.noteCellSize(width: 9, height: 20)
        XCTAssertEqual(grid.layout.geometry.cell, PaneSize(width: 9, height: 20))
    }

    func testPlaceholderSnapshotsDoNotOfferLiveStatusToggle() {
        func snapshot(placeholder: Bool) -> WorkerSlotSnapshot {
            WorkerSlotSnapshot(
                slotId: 1, runId: "run-a", summary: nil, taskTitle: nil, session: nil,
                liveStatusEnabled: true, live: nil, isPlaceholder: placeholder, cachedPersona: nil
            )
        }
        XCTAssertTrue(snapshot(placeholder: false).offersLiveStatusToggle)
        XCTAssertFalse(snapshot(placeholder: true).offersLiveStatusToggle)
    }

    func testAgentTypeFromWire() {
        XCTAssertEqual(AgentType.fromWire("coding"), .coding)
        XCTAssertEqual(AgentType.fromWire("review"), .review)
        XCTAssertEqual(AgentType.fromWire(nil), .unknown)
        XCTAssertEqual(AgentType.fromWire("time-travel"), .unknown)
        XCTAssertEqual(AgentType.unknown.badgeLabel, "Unknown")
    }

    func testQuietTaskAndCellCallbackWaitForResizeBoundary() async throws {
        let grid = AgentsGridModel(geometry: PaneGeometry(
            area: PaneSize(width: 800, height: 600),
            cell: PaneSize(width: 6, height: 13)
        ))
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
                   viewers: [:], interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16)
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
                       viewers: workspace.viewersByRunId, interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16)
            // Every attached surface stays mounted whatever the layout does.
            XCTAssertEqual(Set(grid.renderRunIds(attached: workspace.viewersByRunId.keys)), Set(sessions.keys))
        }
        apply(0) // Attached before membership.
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys).count, 9)
        XCTAssertFalse(grid.placement(for: "run-3", area: CGSize(width: 400, height: 200)).visible)
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
        // Released runs keep their surfaces mounted, hidden, until detach.
        XCTAssertEqual(Set(grid.renderRunIds(attached: workspace.viewersByRunId.keys)), Set((0..<9).map { "run-\($0)" }))
        for index in 1..<9 {
            XCTAssertFalse(grid.placement(for: "run-\(index)", area: CGSize(width: 200, height: 200)).visible)
        }
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

    /// A live member on a page that becomes
    /// visible gets a viewer attached. The grid names the viewer-less
    /// members on the selected page, the workspace asks the engine once per
    /// run, the engine answers with `AttachWorkerPane`, and the next apply
    /// places the terminal in the same cell.
    func testLiveMemberOnVisiblePageGetsViewerAttached() {
        let workspace = WorkersWorkspaceModel()
        let grid = AgentsGridModel(isVisible: false)
        let state = makeState(runId: "run-a")
        grid.apply(
            states: [state], viewers: workspace.viewersByRunId,
            interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false
        )
        XCTAssertEqual(grid.viewerlessRunIdsOnSelectedPage, ["run-a"])
        guard case .occupied(let before) = grid.layout.currentPage?.cells.first?.content else {
            return XCTFail("the live member occupies the first cell")
        }
        XCTAssertFalse(before.hasViewer)

        // Stand in for the engine: the request comes back as AttachWorkerPane.
        var requests: [String] = []
        workspace.onViewerAttachRequested = { [weak workspace] runId in
            requests.append(runId)
            _ = workspace?.attachWorkerPane(self.makeAttach(runId: runId, slot: 1))
        }
        grid.onViewerAttachRequested = { [weak workspace] runs in
            workspace?.requestViewerAttach(forRuns: runs)
        }
        grid.requestViewersForVisiblePage()
        XCTAssertTrue(requests.isEmpty)
        grid.setVisible(true)
        XCTAssertEqual(requests, ["run-a"])
        XCTAssertNotNil(workspace.viewersByRunId["run-a"]?.session)

        grid.apply(
            states: [state], viewers: workspace.viewersByRunId,
            interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false
        )
        XCTAssertTrue(grid.viewerlessRunIdsOnSelectedPage.isEmpty)
        guard case .occupied(let after) = grid.layout.currentPage?.cells.first?.content else {
            return XCTFail("the member keeps its cell once attached")
        }
        XCTAssertTrue(after.hasViewer)
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "run-a"), 0)
    }

    /// Only the selected page's members are offered for a re-attach
    /// request; switching pages offers the next page's.
    func testViewerlessRunIdsFollowTheSelectedPage() {
        let workspace = WorkersWorkspaceModel()
        let grid = AgentsGridModel(
            policy: PaneCapacityPolicy(minColumns: 1, minRows: 1, headerHeight: 0, maxPanesPerPage: 8, gap: 0, deadBand: 0),
            geometry: PaneGeometry(area: PaneSize(width: 400, height: 200), cell: PaneSize(width: 100, height: 100))
        )
        _ = workspace.attachWorkerPane(makeAttach(runId: "run-0", slot: 1))
        grid.apply(
            states: (0..<9).map { makeState(runId: "run-\($0)", slotId: $0 + 1, startedAt: "2026-10-09T00:00:0\($0)Z") },
            viewers: workspace.viewersByRunId,
            interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.pageCount, 2)
        XCTAssertEqual(grid.viewerlessRunIdsOnSelectedPage, (1..<8).map { "run-\($0)" })
        grid.selectPage(1)
        XCTAssertEqual(grid.viewerlessRunIdsOnSelectedPage, ["run-8"])
    }

    func testClearingFilterRequestsNewlyVisibleRunOnSamePage() {
        let workspace = WorkersWorkspaceModel()
        let grid = AgentsGridModel(
            policy: PaneCapacityPolicy(minColumns: 1, minRows: 1, headerHeight: 0, maxPanesPerPage: 8, gap: 0, deadBand: 0),
            geometry: PaneGeometry(area: PaneSize(width: 400, height: 200), cell: PaneSize(width: 100, height: 100))
        )
        var requests: [String] = []
        workspace.onViewerAttachRequested = { requests.append($0) }
        grid.onViewerAttachRequested = { workspace.requestViewerAttach(forRuns: $0) }
        grid.setFilter(PaneFilter(projects: [.project(id: "visible")]))
        grid.apply(
            states: [
                makeState(runId: "shown", projectId: "visible"),
                makeState(runId: "hidden", slotId: 2, projectId: "hidden"),
                makeState(runId: "exited", slotId: 3, activity: .terminated)
            ], viewers: [:],
            interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false
        )
        XCTAssertEqual(requests, ["shown"])
        let page = grid.layout.selectedPage
        grid.clearFilter()
        XCTAssertEqual(grid.layout.selectedPage, page)
        XCTAssertEqual(requests, ["shown", "hidden"])
        grid.requestViewersForVisiblePage()
        XCTAssertEqual(requests, ["shown", "hidden"])
        XCTAssertFalse(grid.viewerlessRunIdsOnSelectedPage.contains("exited"))
    }

    private func roomyGrid() -> AgentsGridModel {
        AgentsGridModel(
            policy: PaneCapacityPolicy(minColumns: 1, minRows: 1, headerHeight: 0, maxPanesPerPage: 8, gap: 0, deadBand: 0),
            geometry: PaneGeometry(area: PaneSize(width: 400, height: 200), cell: PaneSize(width: 100, height: 100))
        )
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
