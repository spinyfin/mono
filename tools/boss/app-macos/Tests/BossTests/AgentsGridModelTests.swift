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

    func testJobWaitsAndNeedsInputAreCountedFromLiveStates() {
        let grid = AgentsGridModel(isVisible: true)
        grid.apply(
            states: [
                makeState(runId: "job-1", slotId: 1, activity: .idle, waitReason: "bazel test"),
                makeState(runId: "job-2", slotId: 2, activity: .idle, waitReason: "ci"),
                makeState(runId: "ask", slotId: 3, activity: .waitingForInput),
                // Mid-turn prompt: the engine keeps waitingForInput and the reason.
                makeState(runId: "mid-turn", slotId: 4, activity: .waitingForInput, waitReason: "bazel"),
                // Resumed worker with a leftover wait is neither.
                makeState(runId: "resumed", slotId: 5, activity: .working, waitReason: "bazel"),
                makeState(runId: "plain-idle", slotId: 6, activity: .idle),
            ],
            viewers: [:],
            interactivePoolSize: 16,
            automationPoolSize: 8,
            reviewPoolSize: 16,
            feedUnavailable: false
        )
        XCTAssertEqual(grid.layout.waitingOnJobCount, 2)
        XCTAssertEqual(grid.layout.waitingCount, 2)
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

    func testTypeFilterOptionsMatchBadgeVerbs() {
        let grid = AgentsGridModel()
        XCTAssertEqual(grid.typeOptions.map(\.badgeLabel), AgentType.filterLabels)
        // Every verb a pane can display is a filter option, worded identically.
        let shown: [(String?, String?, String?)] = [
            ("coding", nil, nil), ("design", nil, nil), ("review", nil, nil),
            ("review", "consolidation", nil), ("review", nil, "pr_review_guide"),
            ("automation", nil, nil), ("answer", nil, nil), (nil, nil, nil),
        ]
        let verbs = Set(shown.map { AgentType.resolve(wire: $0.0, reviewStage: $0.1, executionKind: $0.2).badgeLabel })
        XCTAssertEqual(verbs, Set(grid.typeOptions.map(\.badgeLabel)))
        XCTAssertEqual(Set(grid.typeOptions.map(\.badgeLabel)).count, AgentType.allCases.count)
        XCTAssertTrue(verbs.contains("Guiding"))
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
                liveStatusEnabled: true, live: nil, isPlaceholder: placeholder, cachedPersona: nil, viewerAttachFailure: nil
            )
        }
        XCTAssertTrue(snapshot(placeholder: false).offersLiveStatusToggle)
        XCTAssertFalse(snapshot(placeholder: true).offersLiveStatusToggle)
        XCTAssertEqual(snapshot(placeholder: false).showsLiveStatusToggle, snapshot(placeholder: false).offersLiveStatusToggle)
        XCTAssertEqual(snapshot(placeholder: true).showsLiveStatusToggle, snapshot(placeholder: true).offersLiveStatusToggle)
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
        let states = [
            makeState(runId: "a", slotId: 1, agentType: "coding", projectId: "p1", projectName: "Alpha"),
            makeState(runId: "b", slotId: 2, activity: .waitingForInput, agentType: "review", projectId: "p2", projectName: "Beta"),
            makeState(runId: "c", slotId: 3, agentType: "mystery"),
        ]
        grid.apply(states: states, viewers: workspace.viewersByRunId, interactivePoolSize: 16,
                   automationPoolSize: 8, reviewPoolSize: 16)
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
        let mounted = grid.renderRunIds(attached: workspace.viewersByRunId.keys)
        XCTAssertEqual(mounted, ["a", "b", "c"])
        XCTAssertEqual(Set(mounted.filter { grid.layout.logicalCellIndex(ofRun: $0) == nil }), ["b", "c"])
        for run in ["b", "c"] {
            XCTAssertFalse(grid.placement(for: run, area: CGSize(width: 800, height: 600)).visible)
        }

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
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["a", "b", "c"])
        for run in ["a", "b", "c"] {
            XCTAssertNotNil(grid.layout.logicalCellIndex(ofRun: run))
        }
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

        let seed = LiveWorkerStateStore()
        seed.update(states: [runA, runB])
        grid.sync(isVisible: true, workspace: workspace, liveStates: seed)
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "run-a"), 0)
        grid.sync(isVisible: false, workspace: workspace, liveStates: seed)

        // Hidden: the engine finalizes run-a and drops it from the feed.
        let store = LiveWorkerStateStore()
        store.update(states: [runB])
        grid.sync(isVisible: false, workspace: workspace, liveStates: store)
        XCTAssertNil(grid.layout.logicalCellIndex(ofRun: "run-a"))
        XCTAssertFalse(grid.layout.pages.contains { page in
            page.cells.contains { if case .placeholder = $0.content { return true } else { return false } }
        }, "hidden releases compact away, no placeholder")
        // The surface stays mounted (detach pending) but is a finished pane.
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-a", "run-b"])
        let pending = render(grid, runId: "run-a", workspace: workspace, store: store)
        XCTAssertTrue(pending.isPlaceholder)
        XCTAssertTrue(pending.session === sessionA)
        XCTAssertEqual(pending.bodyKind, .placeholder)
        XCTAssertEqual(pending.headerTitle, "Riker finished")
        XCTAssertNil(pending.headerBadge)
        XCTAssertEqual(pending.statusPillLabel, "Finished")

        // Hidden: the detach arrives. The run is no longer rendered at all.
        assertSuccess(workspace.detachWorkerPane(runId: "run-a"))
        grid.sync(isVisible: false, workspace: workspace, liveStates: store)
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-b"])

        // Back to Agents: current membership, packed, nothing stale.
        grid.sync(isVisible: true, workspace: workspace, liveStates: store)
        XCTAssertEqual(grid.layout.visibleCount, 1)
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "run-b"), 0)
        XCTAssertFalse(grid.layout.isTidyHighlighted)
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-b"])
        let liveB = render(grid, runId: "run-b", workspace: workspace, store: store)
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
        let seed = LiveWorkerStateStore()
        seed.update(states: [runA, runB])
        grid.sync(isVisible: true, workspace: workspace, liveStates: seed)

        let store = LiveWorkerStateStore()
        store.update(states: [runB])
        grid.sync(isVisible: true, workspace: workspace, liveStates: store)
        assertSuccess(workspace.detachWorkerPane(runId: "run-a"))
        grid.sync(isVisible: true, workspace: workspace, liveStates: store)

        let page = try XCTUnwrap(grid.layout.currentPage)
        XCTAssertEqual(page.cells[0].content, .placeholder(runId: "run-a"))
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-a", "run-b"])
        let finished = render(grid, runId: "run-a", workspace: workspace, store: store)
        XCTAssertEqual(finished.bodyKind, .placeholder)
        XCTAssertEqual(finished.headerTitle, "Riker finished")
        XCTAssertNil(finished.headerBadge)
        XCTAssertEqual(finished.statusPillLabel, "Finished")
        XCTAssertFalse(finished.showsLiveStatusToggle)
        XCTAssertFalse(finished.isViewerMissing)
    }

    /// Visible release before the detach arrives: the placed cell is the
    /// finished placeholder while the surface stays attached, and a later
    /// detach leaves the placeholder intact.
    func testVisibleReleaseBeforeDetachShowsPlaceholderWithSurfaceAttached() throws {
        let workspace = WorkersWorkspaceModel()
        let grid = roomyGrid()
        let runA = makeState(runId: "run-a", slotId: 1, name: "Riker")
        let runB = makeState(runId: "run-b", slotId: 2, name: "Data", startedAt: "2026-10-09T00:00:01Z")
        _ = workspace.attachWorkerPane(makeAttach(runId: "run-a", slot: 1))
        _ = workspace.attachWorkerPane(makeAttach(runId: "run-b", slot: 2))
        let sessionA = try XCTUnwrap(workspace.viewersByRunId["run-a"]?.session)
        let store = LiveWorkerStateStore()
        store.update(states: [runA, runB])
        grid.sync(isVisible: true, workspace: workspace, liveStates: store)

        store.update(states: [runB])
        grid.sync(isVisible: true, workspace: workspace, liveStates: store)

        let page = try XCTUnwrap(grid.layout.currentPage)
        XCTAssertEqual(page.cells[0].content, .placeholder(runId: "run-a"))
        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-a", "run-b"])
        let pending = render(grid, runId: "run-a", workspace: workspace, store: store)
        XCTAssertTrue(pending.session === sessionA, "surface stays attached until the detach")
        XCTAssertEqual(pending.bodyKind, .placeholder)
        XCTAssertEqual(pending.headerTitle, "Riker finished")
        let liveB = render(grid, runId: "run-b", workspace: workspace, store: store)
        XCTAssertEqual(liveB.bodyKind, .terminal)

        assertSuccess(workspace.detachWorkerPane(runId: "run-a"))
        grid.sync(isVisible: true, workspace: workspace, liveStates: store)
        XCTAssertEqual(try XCTUnwrap(grid.layout.currentPage).cells[0].content, .placeholder(runId: "run-a"))
        let detached = render(grid, runId: "run-a", workspace: workspace, store: store)
        XCTAssertNil(detached.session)
        XCTAssertEqual(detached.bodyKind, .placeholder)
        XCTAssertEqual(detached.headerTitle, "Riker finished")
    }

    /// A viewer attached before its first live-state snapshot is never
    /// placed in the layout: it is mounted hidden and has no cell to draw
    /// a finished card in.
    func testAttachBeforeSnapshotStaysUnplaced() {
        let workspace = WorkersWorkspaceModel()
        let grid = roomyGrid()
        _ = workspace.attachWorkerPane(makeAttach(runId: "run-a", slot: 1))
        grid.sync(isVisible: true, workspace: workspace, liveStates: LiveWorkerStateStore())

        XCTAssertNil(grid.layout.logicalCellIndex(ofRun: "run-a"))
        XCTAssertEqual(grid.unplacedViewers.compactMap(\.runId), ["run-a"])
        XCTAssertTrue(grid.layout.isEmpty)
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
        grid.sync(isVisible: true, workspace: workspace, liveStates: store)

        // Engine frees slot 1 and spawns run-c into it before the old
        // viewer's detach has landed: the attach is refused as SlotBusy,
        // the engine detaches the stale viewer, then retries.
        store.update(states: [])
        grid.sync(isVisible: true, workspace: workspace, liveStates: store)
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
        grid.sync(isVisible: true, workspace: workspace, liveStates: store)

        XCTAssertEqual(grid.renderRunIds(attached: workspace.viewersByRunId.keys), ["run-c"])
        XCTAssertEqual(grid.layout.logicalCellIndex(ofRun: "run-c"), 0)
        XCTAssertNil(grid.layout.logicalCellIndex(ofRun: "run-a"))
        let liveC = render(grid, runId: "run-c", workspace: workspace, store: store)
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
        let workspace = WorkersWorkspaceModel()
        let store = LiveWorkerStateStore()
        let runA = makeState(runId: "run-a", slotId: 1, name: "Riker")
        let runB = makeState(runId: "run-b", slotId: 2, name: "Data", startedAt: "2026-10-09T00:00:01Z")
        store.update(states: [runA, runB])
        grid.sync(isVisible: true, workspace: workspace, liveStates: store)
        store.update(states: [runB])
        grid.sync(isVisible: true, workspace: workspace, liveStates: store)
        let runC = makeState(runId: "run-c", slotId: 1, name: "Worf", startedAt: "2026-10-09T00:00:05Z")
        store.update(states: [runB, runC])
        grid.sync(isVisible: true, workspace: workspace, liveStates: store)

        let page = try XCTUnwrap(grid.layout.currentPage)
        XCTAssertEqual(page.cells[0].content, .occupied(PaneMember(
            runId: "run-c", startedAt: try XCTUnwrap(WorkerStaleness.parse("2026-10-09T00:00:05Z")),
            project: .unfiled, type: .coding, isWaiting: false, hasViewer: false
        )))
        XCTAssertEqual(grid.renderRunIds(attached: [String]()), ["run-b", "run-c"])
        XCTAssertNil(grid.personaName(for: "run-a"))
        let finishedA = render(grid, runId: "run-a", workspace: workspace, store: store)
        XCTAssertTrue(finishedA.isPlaceholder)
        XCTAssertNil(finishedA.headerBadge)
    }

    private func assertSuccess(_ result: EngineReleaseResult, file: StaticString = #filePath, line: UInt = #line) {
        guard case .success = result else {
            XCTFail("expected detach success, got \(result)", file: file, line: line)
            return
        }
    }

    private func render(
        _ grid: AgentsGridModel,
        runId: String,
        workspace: WorkersWorkspaceModel,
        store: LiveWorkerStateStore
    ) -> WorkerSlotSnapshot {
        grid.renderSnapshot(runId: runId, workspace: workspace, liveStates: store, liveStatusDisabledSlotIDs: [])
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
                makeState(runId: "exited", slotId: 3, activity: .terminated),
                makeState(runId: "errored", slotId: 4, activity: .errored)
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
        XCTAssertFalse(grid.viewerlessRunIdsOnSelectedPage.contains("errored"))
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
        projectName: String? = nil,
        waitReason: String? = nil
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
            waitReason: waitReason,
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
