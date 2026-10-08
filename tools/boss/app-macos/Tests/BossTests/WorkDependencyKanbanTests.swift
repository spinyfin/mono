import AppKit
import Combine
import SwiftUI
import XCTest
@testable import Boss

/// Drives the kanban-side dependency surfaces (chain badge, drag
/// refusal, popover Dependencies subsection) by populating
/// `ChatViewModel`'s published state with a synthetic product and
/// asserting the helpers that the views read. The view code itself
/// is a thin reflection of these helpers — covering them is what the
/// design's "snapshot tests for the badge state and the empty /
/// populated dependency lists" boils down to without a snapshot
/// library wired into the package.
@MainActor
final class WorkDependencyKanbanTests: XCTestCase {
    /// An auto-blocked task — engine-set status, unsatisfied prereq —
    /// must trip the chain-badge predicate. Manual-block parity is
    /// covered separately so a regression that conflates the two
    /// shows up as a clear failing case rather than a silent miss.
    func testAutoBlockedTaskTripsBadgePredicate() {
        let model = makeFixture()
        guard let dependent = model.taskByName("Phase 4") else {
            XCTFail("expected fixture to include the gated task"); return
        }
        XCTAssertTrue(model.isAutoBlocked(dependent))
        XCTAssertEqual(model.gatingPrereqs(for: dependent.id).map(\.title), ["Phase 2"])
    }

    /// A human-set blocked row keeps the lane but loses the chain
    /// badge — design Q7 explicitly carves manual blocks out so the
    /// icon doesn't double up with the lane label.
    func testManualBlockHidesChainBadge() {
        let model = makeFixture()
        model.upsertTaskForTest(
            id: "task_manual",
            name: "Manual block",
            status: "blocked",
            lastStatusActor: "human"
        )
        guard let manual = model.taskByName("Manual block") else {
            XCTFail("expected manual-block fixture"); return
        }
        XCTAssertFalse(model.isAutoBlocked(manual))
    }

    /// `dependencyPrereqs` and `dependencyDependents` underpin the
    /// popover Dependencies subsection. An ungated chore should
    /// return empty lists so the subsection collapses cleanly per the
    /// design.
    func testDependencyListsAreEmptyForUngatedItem() {
        let model = makeFixture()
        guard let lone = model.taskByName("Phase 1") else {
            XCTFail("expected fixture"); return
        }
        XCTAssertEqual(model.dependencyPrereqs(for: lone.id), [])
        XCTAssertEqual(model.dependencyDependents(for: lone.id), [])
    }

    /// Populated lists must surface both incoming and outgoing edges
    /// joined against the work tree's titles and statuses so the
    /// popover can render hyperlinks instead of bare ids.
    func testDependencyListsExposeIncomingAndOutgoingEdges() {
        let model = makeFixture()
        guard let prereq = model.taskByName("Phase 2"),
              let dependent = model.taskByName("Phase 4")
        else {
            XCTFail("expected fixture"); return
        }
        XCTAssertEqual(model.dependencyDependents(for: prereq.id).map(\.title), ["Phase 4"])
        XCTAssertEqual(model.dependencyPrereqs(for: dependent.id).map(\.title), ["Phase 2"])
        XCTAssertEqual(model.dependencyPrereqs(for: dependent.id).first?.status, "active")
    }

    /// An archived task prereq must satisfy gating the same as a done
    /// one — mirrors the engine's `status_satisfies` rule, which treats
    /// `done` and `archived` identically for every work item kind.
    /// Regression coverage for the app's hand-written mirrors of that
    /// rule falling out of sync after the engine started accepting
    /// `archived` as a satisfying status.
    func testArchivedPrereqSatisfiesGating() {
        let model = makeFixture()
        guard let phase2 = model.taskByName("Phase 2"),
              let phase4 = model.taskByName("Phase 4")
        else {
            XCTFail("expected fixture tasks"); return
        }
        model.upsertTaskForTest(
            id: phase2.id,
            name: phase2.name,
            status: "archived",
            lastStatusActor: "human"
        )
        model.invalidateWorkCache(.dependencies)
        XCTAssertFalse(model.isAutoBlocked(phase4))
        XCTAssertEqual(model.gatingPrereqs(for: phase4.id), [])
        XCTAssertEqual(model.dependencyPrereqs(for: phase4.id).map(\.status), ["archived"])
    }

    /// Drops are forwarded to the engine; a refusal bounces the card back
    /// and surfaces the engine's reason verbatim in the inline notice.
    func testAttemptDropForwardsGatedDragAndSurfacesEngineRefusal() {
        let model = makeFixture()
        guard let dependent = model.taskByName("Phase 4") else {
            XCTFail("expected fixture"); return
        }
        let origin = model.effectiveBoardColumn(for: dependent)
        XCTAssertNotEqual(origin, .doing)

        let accepted = model.attemptDrop(dependent.id, onColumn: .doing, group: nil)
        XCTAssertTrue(accepted, "a gated drop is the engine's call, not the client's")
        XCTAssertNil(model.dragRefusalNotice, "no client-side refusal before the engine answers")
        XCTAssertEqual(
            model.effectiveBoardColumn(for: dependent), .doing,
            "the card is placed optimistically while the engine decides"
        )

        // The engine's reply for a genuinely gated row.
        let reason = "cannot move \(dependent.id) to active: gated by [task_p2] (use `boss <kind> depend rm` to remove)"
        model.applyEventForTest(.workError(message: reason, requestId: nil))

        XCTAssertEqual(model.effectiveBoardColumn(for: dependent), origin, "a refused drop bounces back")
        XCTAssertEqual(model.dragRefusalNotice?.taskID, dependent.id)
        XCTAssertEqual(model.dragRefusalNotice?.message, reason, "the engine's reason is surfaced verbatim")
        XCTAssertNil(model.workErrorMessage, "a kanban refusal is inline, not modal")
    }

    /// The display rule mirrors the engine's `status_satisfies_for_dependent`:
    /// `done`/`archived` satisfy everyone; `in_review` additionally
    /// satisfies a same-chain `revision` dependent (its prerequisite's commits are
    /// pushed and the PR is open — all the next writer needs).
    func testPrerequisiteStatusSatisfiesMirrorsEngineRule() {
        for kind in ["task", "chore", "revision", "project_task"] {
            XCTAssertTrue(ChatViewModel.prerequisiteStatusSatisfies("done", dependentKind: kind), kind)
            XCTAssertTrue(ChatViewModel.prerequisiteStatusSatisfies("archived", dependentKind: kind), kind)
            XCTAssertFalse(ChatViewModel.prerequisiteStatusSatisfies("todo", dependentKind: kind), kind)
            XCTAssertFalse(ChatViewModel.prerequisiteStatusSatisfies("active", dependentKind: kind), kind)
            XCTAssertFalse(ChatViewModel.prerequisiteStatusSatisfies("blocked", dependentKind: kind), kind)
        }
        XCTAssertTrue(ChatViewModel.prerequisiteStatusSatisfies("in_review", dependentKind: "revision", sameRevisionChain: true))
        XCTAssertFalse(ChatViewModel.prerequisiteStatusSatisfies("in_review", dependentKind: "revision"))
        XCTAssertFalse(ChatViewModel.prerequisiteStatusSatisfies("in_review", dependentKind: "chore"))
        XCTAssertFalse(ChatViewModel.prerequisiteStatusSatisfies("in_review", dependentKind: "task"))
        XCTAssertFalse(ChatViewModel.prerequisiteStatusSatisfies("in_review", dependentKind: nil))
    }

    func testMixedKindDiamondFrontierIsIndependentOfEdgeOrder() {
        let model = makeFixture()
        for (id, status, kind) in [
            ("revision", "blocked", "revision"),
            ("chore", "blocked", "chore"),
            ("shared", "in_review", "revision")
        ] {
            model.upsertTaskForTest(id: id, name: id, status: status, lastStatusActor: "engine", kind: kind)
        }
        let edges = [
            WorkItemDependency(dependentID: "revision", prerequisiteID: "shared", relation: "blocks"),
            WorkItemDependency(dependentID: "revision", prerequisiteID: "chore", relation: "blocks"),
            WorkItemDependency(dependentID: "chore", prerequisiteID: "shared", relation: "blocks")
        ]
        for orderedEdges in [edges, Array(edges.reversed())] {
            model.dependenciesByProductID["prod_test"] = orderedEdges
            model.invalidateWorkCache(.dependencies)
            XCTAssertEqual(model.actionablePrereqFrontier(for: "revision"), ["shared"])
        }
    }

    /// A CI-fix revision behind an in_review sibling has no gating label;
    /// a chore behind the same prerequisite remains gated.
    func testRevisionIsNotLabelledGatedByInReviewPrerequisite() {
        let model = makeFixture()
        guard let root = model.taskByName("Phase 2") else {
            XCTFail("expected fixture"); return
        }
        model.upsertTaskForTest(
            id: "task_rev_findings",
            name: "Address review findings",
            status: "in_review",
            lastStatusActor: "engine",
            kind: "revision",
            parentTaskId: root.id,
            revisionSeq: 1
        )
        model.upsertTaskForTest(
            id: "task_rev_cifix",
            name: "Fix failing CI",
            status: "blocked",
            lastStatusActor: "engine",
            kind: "revision",
            parentTaskId: root.id,
            revisionSeq: 2
        )
        model.upsertTaskForTest(
            id: "task_chore_behind",
            name: "Chore behind the findings revision",
            status: "blocked",
            lastStatusActor: "engine",
            kind: "chore"
        )
        model.dependenciesByProductID["prod_test", default: []].append(contentsOf: [
            WorkItemDependency(dependentID: "task_rev_cifix", prerequisiteID: "task_rev_findings", relation: "blocks"),
            WorkItemDependency(dependentID: "task_chore_behind", prerequisiteID: "task_rev_findings", relation: "blocks"),
        ])
        model.invalidateWorkCache(.dependencies)
        guard let ciFix = model.taskByName("Fix failing CI"),
              let chore = model.taskByName("Chore behind the findings revision")
        else {
            XCTFail("expected injected rows"); return
        }

        XCTAssertEqual(model.gatingPrereqs(for: ciFix.id), [], "an in_review sibling does not gate a revision")
        XCTAssertEqual(model.gatingPrereqsByTaskID[ciFix.id] ?? [], [], "cached view agrees with the live helper")
        XCTAssertFalse(model.isAutoBlocked(ciFix), "no chain badge for a startable revision")
        XCTAssertNil(model.blockedByLabel(for: ciFix), "no \"Blocked by\" text for a startable revision")
        XCTAssertEqual(
            model.dependencyPrereqs(for: ciFix.id).map(\.title), ["Address review findings"],
            "the edge itself is still listed in the popover"
        )

        XCTAssertEqual(model.gatingPrereqs(for: chore.id).map(\.title), ["Address review findings"])
        XCTAssertEqual(model.gatingPrereqsByTaskID[chore.id]?.map(\.title), ["Address review findings"])
        XCTAssertTrue(model.isAutoBlocked(chore), "a chore behind an in_review row is still gated")
        XCTAssertEqual(model.blockedByLabel(for: chore), "Address review findings")

        model.upsertTaskForTest(
            id: "task_unrelated", name: "Unrelated prerequisite", status: "in_review",
            lastStatusActor: "engine", kind: "chore"
        )
        model.dependenciesByProductID["prod_test", default: []].append(
            WorkItemDependency(dependentID: ciFix.id, prerequisiteID: "task_unrelated", relation: "blocks")
        )
        model.invalidateWorkCache(.dependencies)
        XCTAssertEqual(model.gatingPrereqs(for: ciFix.id).map(\.id), ["task_unrelated"])
        XCTAssertEqual(model.gatingPrereqsByTaskID[ciFix.id]?.map(\.id), ["task_unrelated"])
        XCTAssertEqual(model.blockedByLabel(for: ciFix), "Unrelated prerequisite")
        XCTAssertTrue(model.isAutoBlocked(ciFix))
        XCTAssertEqual(model.actionablePrereqFrontier(for: ciFix.id), ["task_unrelated"])
    }

    /// Default grouping (`.none`) renders the project badge on the
    /// card so the reader can tell which project a task belongs to
    /// without expanding it.
    func testCardProjectBadgeShownWhenUngrouped() {
        let model = makeFixture()
        model.workBoardGrouping = .none
        guard let task = model.taskByName("Phase 2") else {
            XCTFail("expected fixture"); return
        }
        XCTAssertEqual(model.cardProjectBadge(for: task), "Test Project")
    }

    /// Grouping by project promotes the project name to the lane
    /// header, so the per-card badge would just duplicate it. The
    /// helper must suppress it across every card in that mode.
    func testCardProjectBadgeHiddenWhenGroupedByProject() {
        let model = makeFixture()
        model.workBoardGrouping = .project
        guard let task = model.taskByName("Phase 2") else {
            XCTFail("expected fixture"); return
        }
        XCTAssertNil(model.cardProjectBadge(for: task))
    }

    /// Chores have no project so the badge was already absent; the
    /// helper must hold that line regardless of grouping mode.
    func testCardProjectBadgeAlwaysNilForChores() {
        let model = makeFixture()
        let productID = model.products.first?.id ?? "prod_test"
        let chore = WorkTask(
            id: "chore_test",
            productID: productID,
            projectID: nil,
            kind: "chore",
            name: "Tidy",
            description: "",
            status: "active",
            priority: "medium",
            ordinal: 1,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "human"
        )
        model.choresByProductID = [productID: [chore]]
        model.workBoardGrouping = .none
        XCTAssertNil(model.cardProjectBadge(for: chore))
        model.workBoardGrouping = .project
        XCTAssertNil(model.cardProjectBadge(for: chore))
    }

    /// An active task with `blocked_reason=dependency` must surface its
    /// prereqs via `dependencyPrereqs` so the card can render "Waiting
    /// on: <name>". This mirrors the real repro: `status=active`,
    /// `blocked_reason=dependency` — the engine left the field set after
    /// the last block evaluation.
    func testActiveTaskWithBlockedReasonDependencyExposesPrereqs() {
        let model = makeFixture()
        guard let dependent = model.taskByName("Phase 4") else {
            XCTFail("expected fixture to include the gated task"); return
        }
        // Phase 4 is status=blocked in the fixture; inject an active
        // clone with the same dependency edge to exercise the
        // non-blocked-status path.
        let productID = model.products.first?.id ?? "prod_test"
        let projectID = model.projectsByProductID[productID]?.first?.id ?? "proj_test"
        let activeGated = WorkTask(
            id: "task_active_gated",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Active But Gated",
            description: "",
            status: "active",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "engine",
            blockedReason: "dependency"
        )
        var tasks = model.tasksByProjectID[projectID] ?? []
        tasks.append(activeGated)
        model.tasksByProjectID[projectID] = tasks
        model.dependenciesByProductID[productID, default: []].append(
            WorkItemDependency(
                dependentID: activeGated.id,
                prerequisiteID: dependent.id,
                relation: "blocks"
            )
        )
        // dependencyPrereqs drives the "Waiting on:" subtitle — must
        // include the prereq even when the card status is not "blocked".
        let prereqs = model.dependencyPrereqs(for: activeGated.id)
        XCTAssertEqual(prereqs.map(\.title), ["Phase 4"])
    }

    /// Stale dependency block: `blocked_reason=dependency` is set but
    /// the sole prereq is already done. `dependencyPrereqs` must still
    /// return the prereq row (for the "Waiting on:" label) even though
    /// `gatingPrereqs` returns empty (nothing incomplete). The card
    /// uses the full set so the stale block is still visible.
    func testStaleDependencyBlockStillExposesPrereqsForCard() {
        let model = makeFixture()
        guard let done = model.taskByName("Phase 1") else {
            XCTFail("expected fixture"); return
        }
        let productID = model.products.first?.id ?? "prod_test"
        let projectID = model.projectsByProductID[productID]?.first?.id ?? "proj_test"
        let staleGated = WorkTask(
            id: "task_stale_gated",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Stale Gated",
            description: "",
            status: "active",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "engine",
            blockedReason: "dependency"
        )
        var tasks = model.tasksByProjectID[projectID] ?? []
        tasks.append(staleGated)
        model.tasksByProjectID[projectID] = tasks
        model.dependenciesByProductID[productID, default: []].append(
            WorkItemDependency(
                dependentID: staleGated.id,
                prerequisiteID: done.id,
                relation: "blocks"
            )
        )
        // Phase 1 is done — no gating prereqs, but the edge still exists.
        XCTAssertEqual(model.gatingPrereqs(for: staleGated.id), [])
        // The full prereq list is non-empty — the card must show "Waiting on: Phase 1".
        let allPrereqs = model.dependencyPrereqs(for: staleGated.id)
        XCTAssertEqual(allPrereqs.map(\.title), ["Phase 1"])
    }

    /// A manual-block row with no gating edges should still be
    /// movable — the engine accepts a manual unblock once the prereq
    /// set is empty, so the kanban must not pre-empt that.
    func testAttemptDropAcceptsUngatedDragOutOfBlocked() {
        let model = makeFixture()
        model.upsertTaskForTest(
            id: "task_movable",
            name: "Movable",
            status: "blocked",
            lastStatusActor: "human"
        )
        let accepted = model.attemptDrop("task_movable", onColumn: .doing, group: nil)
        XCTAssertTrue(accepted)
        XCTAssertNil(model.dragRefusalNotice)
    }

    // MARK: - Dependency badge frontier

    /// Phase 4 is blocked by Phase 2 (active, no gating prereqs).
    /// The frontier for Phase 4 is exactly {Phase 2} — it is
    /// reachable, unblocked, and open.
    func testFrontierForDirectlyBlockedTask() {
        let model = makeFixture()
        guard let phase2 = model.taskByName("Phase 2"),
              let phase4 = model.taskByName("Phase 4")
        else {
            XCTFail("expected fixture tasks"); return
        }
        let frontier = model.actionablePrereqFrontier(for: phase4.id)
        XCTAssertEqual(frontier, [phase2.id])
    }

    /// Phase 1 is already done, so it is not open. The frontier for
    /// a task blocked only by Phase 1 must be empty (nothing actionable).
    func testFrontierExcludesTerminalPrereqs() {
        let model = makeFixture()
        guard let phase1 = model.taskByName("Phase 1") else {
            XCTFail("expected fixture"); return
        }
        let productID = model.products.first?.id ?? "prod_test"
        let projectID = model.projectsByProductID[productID]?.first?.id ?? "proj_test"
        let staleBlocked = WorkTask(
            id: "task_stale",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Stale Blocked",
            description: "",
            status: "blocked",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "engine"
        )
        var tasks = model.tasksByProjectID[projectID] ?? []
        tasks.append(staleBlocked)
        model.tasksByProjectID[projectID] = tasks
        model.dependenciesByProductID[productID, default: []].append(
            WorkItemDependency(dependentID: staleBlocked.id, prerequisiteID: phase1.id, relation: "blocks")
        )
        let frontier = model.actionablePrereqFrontier(for: staleBlocked.id)
        XCTAssertTrue(frontier.isEmpty, "frontier must be empty when the only prereq is already done")
    }

    /// Three-deep chain: Chore → A (blocked) → B (active, unblocked).
    /// The frontier for Chore is {B} — A is blocked so it is not
    /// actionable yet; B is the reachable leaf that is open and unblocked.
    func testFrontierWalksThroughBlockedIntermediateNodes() {
        let model = makeFixture()
        let productID = model.products.first?.id ?? "prod_test"
        let projectID = model.projectsByProductID[productID]?.first?.id ?? "proj_test"

        let taskB = WorkTask(
            id: "task_b",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Task B",
            description: "",
            status: "active",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "human"
        )
        let taskA = WorkTask(
            id: "task_a",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Task A",
            description: "",
            status: "blocked",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "engine"
        )
        let chore = WorkTask(
            id: "chore_c",
            productID: productID,
            projectID: nil,
            kind: "chore",
            name: "Chore C",
            description: "",
            status: "blocked",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "engine"
        )
        var tasks = model.tasksByProjectID[projectID] ?? []
        tasks.append(contentsOf: [taskB, taskA])
        model.tasksByProjectID[projectID] = tasks
        model.choresByProductID[productID, default: []].append(chore)
        model.dependenciesByProductID[productID, default: []] += [
            WorkItemDependency(dependentID: taskA.id, prerequisiteID: taskB.id, relation: "blocks"),
            WorkItemDependency(dependentID: chore.id, prerequisiteID: taskA.id, relation: "blocks"),
        ]
        let frontier = model.actionablePrereqFrontier(for: chore.id)
        // Task A is blocked (not unblocked), so it is not in the frontier.
        // Task B is active, unblocked — it is the actionable frontier.
        XCTAssertEqual(frontier, [taskB.id])
    }

    /// `setDepBadgeHover` populates `depFrontierHighlightIDs` on enter
    /// and clears it on leave (nil).
    func testSetDepBadgeHoverPopulatesAndClearsHighlightSet() {
        let model = makeFixture()
        guard let phase2 = model.taskByName("Phase 2"),
              let phase4 = model.taskByName("Phase 4")
        else {
            XCTFail("expected fixture tasks"); return
        }
        XCTAssertTrue(model.depFrontierHighlightIDs.isEmpty, "should start empty")
        model.setDepBadgeHover(phase4.id)
        XCTAssertEqual(model.depFrontierHighlightIDs, [phase2.id], "phase2 is the frontier for phase4")
        model.setDepBadgeHover(nil)
        XCTAssertTrue(model.depFrontierHighlightIDs.isEmpty, "should clear on nil")
    }

    // MARK: - Hover-publish idempotence (livelock regression guard)
    //
    // `setDepBadgeHover` / `setRevisionBadgeHover` run on SwiftUI's hover
    // hit-test path, which re-fires at the end of every graph update whose
    // layout moved anything under the pointer. `WorkBoardSectionItemsView`
    // observes the whole `ChatViewModel` and reads both highlight sets, so a
    // `@Published` write that changes nothing still re-evaluates the column,
    // rebuilds every card snapshot, and re-applies the whole `LazyVStack`
    // list — which invalidates the responder tree and enqueues the *next*
    // hover update. Publishing unconditionally closes that loop.
    //
    // These tests assert the gate rather than the value, because the value
    // assertions above already pass with an ungated setter: the ungated bug
    // is invisible to every test that only reads `depFrontierHighlightIDs`.

    /// A hover-exit while nothing is highlighted must not publish.
    /// This is the exact tick SwiftUI replays on every responder rebuild.
    func testRedundantDepBadgeHoverExitDoesNotPublish() {
        let model = makeFixture()
        XCTAssertTrue(model.depFrontierHighlightIDs.isEmpty, "precondition: nothing highlighted")

        var publishes = 0
        let token = model.objectWillChange.sink { _ in publishes += 1 }
        defer { token.cancel() }

        model.setDepBadgeHover(nil)
        model.setDepBadgeHover(nil)

        XCTAssertEqual(publishes, 0, "clearing an already-empty frontier must not fire objectWillChange")
    }

    /// Re-entering the same badge — which a responder rebuild replays
    /// verbatim — must publish once, not once per hover tick.
    func testRepeatedDepBadgeHoverEnterPublishesOnlyOnChange() {
        let model = makeFixture()
        guard let phase4 = model.taskByName("Phase 4") else {
            XCTFail("expected fixture tasks"); return
        }

        var publishes = 0
        let token = model.objectWillChange.sink { _ in publishes += 1 }
        defer { token.cancel() }

        model.setDepBadgeHover(phase4.id)
        XCTAssertEqual(publishes, 1, "the first enter changes the set and must publish")

        model.setDepBadgeHover(phase4.id)
        model.setDepBadgeHover(phase4.id)
        XCTAssertEqual(publishes, 1, "re-entering the same badge must not re-publish")

        model.setDepBadgeHover(nil)
        XCTAssertEqual(publishes, 2, "a genuine exit still publishes so the highlight clears")
    }

    /// Revision hover bypasses the broad model publisher and updates only the
    /// keyed state for the revision card whose chrome changes.
    func testRevisionBadgeHoverPublishesOnlyAffectedCardState() {
        let model = makeFixture()
        guard let phase2 = model.taskByName("Phase 2") else {
            XCTFail("expected fixture tasks"); return
        }
        model.addActiveRevisionForTest(parentID: phase2.id)
        let revisionID = "task_rev_of_\(phase2.id)"
        let revisionState = model.revisionHighlightState(for: revisionID)
        let unrelatedState = model.revisionHighlightState(for: phase2.id)

        var modelPublishes = 0
        var revisionPublishes = 0
        var unrelatedPublishes = 0
        let modelToken = model.objectWillChange.sink { _ in modelPublishes += 1 }
        let revisionToken = revisionState.objectWillChange.sink { _ in revisionPublishes += 1 }
        let unrelatedToken = unrelatedState.objectWillChange.sink { _ in unrelatedPublishes += 1 }
        defer {
            modelToken.cancel()
            revisionToken.cancel()
            unrelatedToken.cancel()
        }

        model.setRevisionBadgeHover(nil)
        XCTAssertEqual(modelPublishes, 0)
        XCTAssertEqual(revisionPublishes, 0)

        model.setRevisionBadgeHover(phase2.id)
        XCTAssertEqual(model.revisionHighlightIDs.count, 1, "the revision must actually highlight")
        XCTAssertTrue(revisionState.isHighlighted)
        XCTAssertEqual(modelPublishes, 0, "revision hover must bypass broad model publication")
        XCTAssertEqual(revisionPublishes, 1, "the affected revision card must publish once")
        XCTAssertEqual(unrelatedPublishes, 0, "unaffected cards must not publish")

        model.setRevisionBadgeHover(phase2.id)
        XCTAssertEqual(revisionPublishes, 1, "re-entering the same badge must not re-publish")

        model.setRevisionBadgeHover(nil)
        XCTAssertFalse(revisionState.isHighlighted)
        XCTAssertEqual(modelPublishes, 0)
        XCTAssertEqual(revisionPublishes, 2, "a genuine exit must clear the affected card")
        XCTAssertEqual(unrelatedPublishes, 0)
    }

    /// A state cell first requested after hover entry must reflect the active
    /// set, which covers an off-screen lazy card becoming mounted mid-hover.
    func testRevisionHighlightStateCreatedDuringHoverStartsHighlighted() {
        let model = makeFixture()
        guard let phase2 = model.taskByName("Phase 2") else {
            XCTFail("expected fixture tasks"); return
        }
        model.addActiveRevisionForTest(parentID: phase2.id)
        model.setRevisionBadgeHover(phase2.id)

        let state = model.revisionHighlightState(for: "task_rev_of_\(phase2.id)")

        XCTAssertTrue(state.isHighlighted)
    }

    /// When the only strong holder of a cell goes away, the registry's weak
    /// entry zeroes and a subsequent `state(for:)` allocates a fresh cell.
    /// Identity is checked via a weak ref (not `ObjectIdentifier`) because
    /// malloc often reuses the freed address for the immediate reallocation.
    func testRevisionHighlightStoreDropsReleasedCells() {
        let store = WorkBoardRevisionHighlightStore()
        weak var weakState: WorkBoardRevisionHighlightState?
        Self.withAllocatedHighlightState(store, taskID: "a") { held in
            weakState = held
            XCTAssertTrue(store.state(for: "a") === held)
            XCTAssertEqual(store.liveRegisteredStateCount, 1)
        }
        XCTAssertNil(
            weakState,
            "cell must deallocate when its only strong holder is gone"
        )
        XCTAssertEqual(
            store.liveRegisteredStateCount,
            0,
            "released cells must not remain reachable via the registry"
        )
        let second = store.state(for: "a")
        XCTAssertEqual(second.taskID, "a")
        // A zeroed weak ref proves this is a new allocation, even if the
        // heap reuses the previous address (where `===` would mislead).
        XCTAssertNil(weakState, "new state(for:) must not resurrect the old cell")
        XCTAssertEqual(store.liveRegisteredStateCount, 1)
    }

    @inline(never)
    private static func withAllocatedHighlightState(
        _ store: WorkBoardRevisionHighlightStore,
        taskID: String,
        body: (WorkBoardRevisionHighlightState) -> Void
    ) {
        let held = store.state(for: taskID)
        body(held)
    }

    /// The 256-entry prune must strip zeroed weak entries and leave live cells
    /// reachable by identity.
    func testRevisionHighlightStorePruneKeepsLiveCells() {
        let store = WorkBoardRevisionHighlightStore()
        let live = store.state(for: "live")
        for index in 0..<300 {
            _ = store.state(for: "temp_\(index)")
        }
        XCTAssertTrue(
            store.state(for: "live") === live,
            "a cell still held strongly must survive the prune sweep"
        )
        // Temps were short-lived: after the sweep only the held cell remains.
        XCTAssertEqual(store.liveRegisteredStateCount, 1)
    }

    /// A genuine revision-badge transition must not rebuild snapshots for
    /// every card in the section. This hosts the real section view and drives
    /// the same model entry point as the badge's `onHover` callback. Also
    /// delivers a burst of repeated enter/exit pairs so a high-frequency
    /// no-op stream cannot quietly reintroduce column-wide rebuilds.
    func testRevisionBadgeHoverDoesNotRebuildColumnSnapshots() {
        let model = makeFixture()
        guard let phase2 = model.taskByName("Phase 2") else {
            XCTFail("expected fixture tasks"); return
        }
        model.addActiveRevisionForTest(parentID: phase2.id)
        for index in 0..<20 {
            model.upsertTaskForTest(
                id: "task_filler_\(index)",
                name: "Filler \(index)",
                status: "active",
                lastStatusActor: "human"
            )
        }
        let items = model.tasksByProjectID.values.flatMap { $0 }
        let frame = NSRect(x: 0, y: 0, width: 420, height: CGFloat(items.count * 180))
        let hosting = NSHostingView(
            rootView: WorkBoardSectionItemsView(
                items: items,
                column: .doing,
                boardStyle: .classic,
                model: model,
                liveStates: model.liveWorkerStates
            )
        )
        hosting.frame = frame
        let window = NSWindow(
            contentRect: frame,
            styleMask: [.titled],
            backing: .buffered,
            defer: false
        )
        window.contentView = hosting
        hosting.layoutSubtreeIfNeeded()
        RunLoop.current.run(until: Date().addingTimeInterval(0.1))

        let before = UIUpdateCounters.shared.currentCounts()
        model.setRevisionBadgeHover(phase2.id)
        RunLoop.current.run(until: Date().addingTimeInterval(0.1))
        hosting.layoutSubtreeIfNeeded()
        let afterFirst = UIUpdateCounters.shared.currentCounts()

        // Burst of repeated enter/exit pairs after the genuine transition.
        for _ in 0..<20 {
            model.setRevisionBadgeHover(phase2.id)
            model.setRevisionBadgeHover(nil)
            model.setRevisionBadgeHover(phase2.id)
        }
        RunLoop.current.run(until: Date().addingTimeInterval(0.1))
        hosting.layoutSubtreeIfNeeded()
        let afterBurst = UIUpdateCounters.shared.currentCounts()
        window.orderOut(nil)

        XCTAssertGreaterThanOrEqual(
            afterFirst.cardSnapshotBuilds,
            before.cardSnapshotBuilds,
            "counter snapshot must not go backwards (flush would trap on UInt64 delta)"
        )
        XCTAssertGreaterThanOrEqual(
            afterFirst.cardBodyEvaluations,
            before.cardBodyEvaluations
        )
        XCTAssertGreaterThanOrEqual(
            afterBurst.cardSnapshotBuilds,
            afterFirst.cardSnapshotBuilds
        )
        let builtForHover = Int(afterFirst.cardSnapshotBuilds - before.cardSnapshotBuilds)
        let bodiesForHover = Int(afterFirst.cardBodyEvaluations - before.cardBodyEvaluations)
        let builtForBurst = Int(afterBurst.cardSnapshotBuilds - afterFirst.cardSnapshotBuilds)

        XCTAssertEqual(builtForHover, 0, "hover state must bypass section-wide snapshot rebuilding")
        XCTAssertGreaterThanOrEqual(bodiesForHover, 1, "the hover update must land on at least one card body")
        XCTAssertLessThan(
            bodiesForHover,
            items.count,
            "a column-wide body re-evaluation would reintroduce scroll jank"
        )
        XCTAssertEqual(
            builtForBurst,
            0,
            "repeated enter/exit after the parent-id gate must not rebuild snapshots"
        )
    }

    // MARK: - Fixture

    /// One product, one project, three tasks (Phase 1 done, Phase 2
    /// active, Phase 4 blocked-by-engine on Phase 2). Mirrors the
    /// shape the engine emits in `WorkTree` so the helpers exercise
    /// the same join semantics they will in production.
    private func makeFixture() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let productID = "prod_test"
        model.products = [
            WorkProduct(
                id: productID,
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-08T00:00:00Z",
                updatedAt: "2026-05-08T00:00:00Z"
            )
        ]
        let projectID = "proj_test"
        model.projectsByProductID = [
            productID: [
                WorkProject(
                    id: projectID,
                    productID: productID,
                    name: "Test Project",
                    slug: "test",
                    description: "",
                    goal: "",
                    status: "active",
                    priority: "medium",
                    createdAt: "2026-05-08T00:00:00Z",
                    updatedAt: "2026-05-08T00:00:00Z",
                    lastStatusActor: "human"
                )
            ]
        ]
        let phase1 = WorkTask(
            id: "task_p1",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Phase 1",
            description: "",
            status: "done",
            priority: "medium",
            ordinal: 1,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "human"
        )
        let phase2 = WorkTask(
            id: "task_p2",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Phase 2",
            description: "",
            status: "active",
            priority: "medium",
            ordinal: 2,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "human"
        )
        let phase4 = WorkTask(
            id: "task_p4",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Phase 4",
            description: "",
            status: "blocked",
            priority: "medium",
            ordinal: 4,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "engine"
        )
        model.tasksByProjectID = [projectID: [phase1, phase2, phase4]]
        model.dependenciesByProductID = [
            productID: [
                WorkItemDependency(
                    dependentID: phase4.id,
                    prerequisiteID: phase2.id,
                    relation: "blocks"
                )
            ]
        ]
        return model
    }
}

// MARK: - Test-only helpers

extension ChatViewModel {
    /// Lookup helper used by the dependency tests so each assertion
    /// can read the fixture by human-readable name without leaking
    /// generated ids into the test bodies.
    fileprivate func taskByName(_ name: String) -> WorkTask? {
        for tasks in tasksByProjectID.values {
            if let match = tasks.first(where: { $0.name == name }) {
                return match
            }
        }
        for chores in choresByProductID.values {
            if let match = chores.first(where: { $0.name == name }) {
                return match
            }
        }
        return nil
    }

    /// Append one active revision row whose chain root is `parentID`,
    /// so `setRevisionBadgeHover(parentID)` has something to highlight.
    fileprivate func addActiveRevisionForTest(parentID: String) {
        upsertTaskForTest(
            id: "task_rev_of_\(parentID)",
            name: "Revision of \(parentID)",
            status: "active",
            lastStatusActor: "human",
            kind: "revision",
            parentTaskId: parentID,
            revisionSeq: 1
        )
    }

    /// Inject (or replace) a task on the fixture's first project.
    /// Lets each test extend the baseline fixture with a single
    /// targeted row (manual block, ungated mover) without rebuilding
    /// the whole tree.
    fileprivate func upsertTaskForTest(
        id: String,
        name: String,
        status: String,
        lastStatusActor: String,
        kind: String = "task",
        parentTaskId: String? = nil,
        revisionSeq: Int? = nil
    ) {
        guard let projectID = projectsByProductID.values.first?.first?.id,
              let productID = projectsByProductID.first?.key
        else {
            XCTFail("upsertTaskForTest called before fixture had a project")
            return
        }
        let task = WorkTask(
            id: id,
            productID: productID,
            projectID: projectID,
            kind: kind,
            name: name,
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: lastStatusActor,
            parentTaskId: parentTaskId,
            revisionSeq: revisionSeq
        )
        var tasks = tasksByProjectID[projectID] ?? []
        if let existing = tasks.firstIndex(where: { $0.id == id }) {
            tasks[existing] = task
        } else {
            tasks.append(task)
        }
        tasksByProjectID[projectID] = tasks
    }
}
