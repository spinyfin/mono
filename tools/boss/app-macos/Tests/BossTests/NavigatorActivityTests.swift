import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class NavigatorActivityTests: XCTestCase {
    func testDraftsSurviveTabSectionModeAndStayPerDocument() {
        let root = NavigatorTestFixture.task()
        let revision = NavigatorTestFixture.task("revision", rootID: "root")
        let model = NavigatorTestFixture.model([root, revision])
        model.isConnected = true
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        XCTAssertEqual(model.navigatorDocument(for: "root"), .currentAsk)

        model.beginNavigatorBriefEdit(revision)
        model.setNavigatorBriefText(revision.id, text: "Current ask draft")
        model.setNavigatorDocument(.original, rootID: "root")
        model.beginNavigatorBriefEdit(root)
        model.setNavigatorBriefText(root.id, text: "Original brief draft")

        model.navigator.tab = .history
        NavigatorTestFixture.snapshot(model, [NavigatorTestFixture.task(section: "backlog"), revision])
        model.setWorkViewMode(.kanban)
        model.setWorkViewMode(.navigator)
        model.navigator.tab = .activity

        XCTAssertEqual(model.navigatorBriefText(for: root), "Original brief draft")
        XCTAssertEqual(model.navigatorBriefText(for: revision), "Current ask draft")
        XCTAssertTrue(model.navigatorBriefIsEditing(root.id))
        XCTAssertTrue(model.navigatorBriefIsEditing(revision.id))
        XCTAssertEqual(model.navigator.selectedRootID, "root")
    }

    func testRemoteChangeOnlyWhileEditingAndSaveClearsIt() {
        var root = NavigatorTestFixture.task()
        let model = NavigatorTestFixture.model([root])
        model.isConnected = true
        model.engine.simulateConnectedForTesting = true
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        model.setNavigatorDocument(.original, rootID: "root")
        model.beginNavigatorBriefEdit(root)
        model.setNavigatorBriefText(root.id, text: "Local draft")
        root.description = "Remote brief"
        NavigatorTestFixture.snapshot(model, [root])
        XCTAssertTrue(model.navigatorBriefChangedRemotely(root))

        XCTAssertTrue(model.saveNavigatorBrief(root))
        XCTAssertFalse(model.navigatorBriefIsEditing(root.id))
        XCTAssertFalse(model.navigatorBriefChangedRemotely(root))

        root.description = "Later remote brief"
        NavigatorTestFixture.snapshot(model, [root])
        XCTAssertFalse(model.navigatorBriefChangedRemotely(root), "Saved drafts must not raise remote-change until editing again")
        XCTAssertEqual(model.navigatorBriefText(for: root), "Later remote brief")
        model.beginNavigatorBriefEdit(root)
        XCTAssertEqual(model.navigatorBriefText(for: root), "Later remote brief")
        model.cancelNavigatorBriefEdit(root)
        XCTAssertEqual(model.navigatorBriefText(for: root), "Later remote brief")
    }

    func testOriginalVersusCurrentAskIsStickyPerRoot() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task(), NavigatorTestFixture.task("revision", rootID: "root")])
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        XCTAssertEqual(model.navigatorDocument(for: "root"), .currentAsk)
        XCTAssertEqual(model.navigatorDocumentTask(for: model.navigatorSelectedTask!).id, "revision")
        model.setNavigatorDocument(.original, rootID: "root")
        NavigatorTestFixture.snapshot(model, [NavigatorTestFixture.task(section: "needs_attention"),
                                              NavigatorTestFixture.task("revision", rootID: "root")])
        XCTAssertEqual(model.navigatorDocument(for: "root"), .original)
        XCTAssertEqual(model.navigatorDocumentTask(for: model.navigatorSelectedTask!).id, "root")
    }

    func testActivityPickerKeepsEndedExecutionAndLiveFlag() {
        let root = activityTask(section: "working", activities: [
            ["execution_id": "execution", "task_id": "revision", "run_id": "run"],
        ])
        let model = NavigatorTestFixture.model([root])
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        XCTAssertEqual(model.navigatorSelectedActivity(for: root)?.runId, "run")
        model.applyEventForTest(.workerLiveStatesList(states: [liveState(runId: "run", activity: .working)]))
        var view = WorkNavigatorActivity(context: context(model), workspace: nil)
        XCTAssertEqual(view.variant, .working)
        XCTAssertTrue(view.isLiveSelected)
        XCTAssertFalse(view.selectedExecutionEnded)

        var moved = activityTask(section: "working", activities: [
            ["execution_id": "execution-2", "task_id": "revision", "run_id": "run-2"],
        ])
        moved.description = root.description
        NavigatorTestFixture.snapshot(model, [moved])
        model.applyEventForTest(.workerLiveStatesList(states: [liveState(runId: "run-2", slotId: 4, activity: .working)]))
        view = WorkNavigatorActivity(context: context(model), workspace: nil)
        XCTAssertEqual(model.navigatorSelectedActivity(for: model.navigatorSelectedTask!)?.executionId, "execution")
        XCTAssertEqual(model.navigatorSelectedActivity(for: model.navigatorSelectedTask!)?.runId, "run")
        XCTAssertEqual(model.navigatorSelectedActivity(for: model.navigatorSelectedTask!)?.taskId, "revision")
        XCTAssertTrue(view.selectedExecutionEnded)
        XCTAssertFalse(view.isLiveSelected)
    }

    func testDeclaredWaitActivitiesStayLiveAndDoNotEndTheRun() {
        let root = activityTask(section: "working", activities: [
            ["execution_id": "execution", "task_id": "revision", "run_id": "run"],
        ])
        let liveCases: [WorkerActivity] = [.waitingOnWork, .waitOverdue, .waitingForInput, .idle]
        for activity in liveCases {
            let model = NavigatorTestFixture.model([root])
            model.selectWorkCard("root")
            model.setWorkViewMode(.navigator)
            model.applyEventForTest(.workerLiveStatesList(states: [liveState(runId: "run", activity: activity)]))
            let view = WorkNavigatorActivity(context: context(model), workspace: nil)
            XCTAssertTrue(view.isLiveSelected, "\(activity) should stay live")
            XCTAssertFalse(view.selectedRunEnded, "\(activity) should not end the run")
        }

        for activity in [WorkerActivity.errored, .terminated] {
            let model = NavigatorTestFixture.model([root])
            model.selectWorkCard("root")
            model.setWorkViewMode(.navigator)
            model.applyEventForTest(.workerLiveStatesList(states: [liveState(runId: "run", activity: activity)]))
            let view = WorkNavigatorActivity(context: context(model), workspace: nil)
            XCTAssertFalse(view.isLiveSelected, "\(activity) is not live")
            XCTAssertTrue(view.selectedRunEnded, "\(activity) ends the run")
        }
    }

    func testRestingVariantsHaveNoLiveIndicator() {
        let cases: [(String, String?, Bool, NavigatorActivityVariant)] = [
            ("backlog", "waiting_dependency", false, .backlog),
            ("backlog", "failed", true, .failed),
            ("backlog", "starting", false, .starting),
            ("landed", nil, false, .landed),
        ]
        for (section, wait, failed, expected) in cases {
            let root = activityTask(section: section, waitKind: wait, failed: failed, merge: expected == .landed)
            let model = NavigatorTestFixture.model([root])
            model.selectWorkCard("root")
            model.setWorkViewMode(.navigator)
            model.applyEventForTest(.workerLiveStatesList(states: [liveState(runId: "unrelated", activity: .working)]))
            let view = WorkNavigatorActivity(context: context(model), workspace: nil)
            XCTAssertEqual(view.variant, expected, "section=\(section) wait=\(wait ?? "nil")")
            XCTAssertFalse(view.isLiveSelected)
            XCTAssertFalse(view.selectedExecutionEnded)
        }
    }

    func testRevisionTransitionKeepsRootAndExposesCurrentAsk() {
        let original = activityTask(section: "working", activities: [
            ["execution_id": "execution", "task_id": "root", "run_id": "run"],
        ])
        let model = NavigatorTestFixture.model([original])
        model.isConnected = true
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        model.beginNavigatorBriefEdit(original)
        model.setNavigatorBriefText(original.id, text: "Kept original draft")
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        XCTAssertEqual(model.navigatorDocument(for: "root"), .original)

        var revised = activityTask(section: "working", activities: [
            ["execution_id": "execution-2", "task_id": "revision", "run_id": "run-2"],
        ], revisions: ["open": 1, "total": 1, "current_task_id": "revision"])
        revised.description = original.description
        let ask = NavigatorTestFixture.task("revision", rootID: "root")
        NavigatorTestFixture.snapshot(model, [revised, ask])
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        XCTAssertEqual(model.navigator.askID, "revision")
        XCTAssertEqual(model.navigatorDocument(for: "root"), .original)
        XCTAssertEqual(model.navigatorBriefText(for: original), "Kept original draft")
        model.setNavigatorDocument(.currentAsk, rootID: "root")
        XCTAssertEqual(model.navigatorDocumentTask(for: model.navigatorSelectedTask!).id, "revision")
        XCTAssertEqual(model.navigatorFamily(of: model.navigatorSelectedTask!).map(\.id), ["root", "revision"])
    }

    func testSessionClaimReusesIdentityAndDoesNotStealFirstResponder() {
        let session = TerminalPaneSession(
            id: "run",
            role: .worker(slot: 1),
            launchSpec: TerminalLaunchSpec(
                fontSize: 12, workingDirectory: NSTemporaryDirectory(), initialInput: ""
            )
        )
        session.claimHost(into: NSView())
        XCTAssertNil(session.hostView, "Claiming without a host must not create a PTY")

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 320),
            styleMask: [.titled], backing: .buffered, defer: false
        )
        let content = NSView(frame: NSRect(x: 0, y: 0, width: 480, height: 320))
        window.contentView = content
        let editor = NSTextView(frame: NSRect(x: 0, y: 0, width: 200, height: 40))
        editor.string = "brief draft"
        let home = NSView(frame: NSRect(x: 0, y: 50, width: 200, height: 120))
        let navigator = NSView(frame: NSRect(x: 220, y: 50, width: 200, height: 120))
        let host = NSView(frame: home.bounds)
        content.addSubview(editor)
        content.addSubview(home)
        content.addSubview(navigator)
        home.addSubview(host)
        session.homeAnchor = home
        window.makeFirstResponder(editor)
        session.reparent(host, into: navigator)
        XCTAssertEqual(host.superview, navigator)
        XCTAssertTrue(window.firstResponder === editor)

        window.makeFirstResponder(host)
        session.reparent(host, into: home)
        XCTAssertEqual(host.superview, home)
        XCTAssertTrue(window.firstResponder === host)
    }

    func testInScopeDependencySelectsWithoutLeavingNavigator() {
        let root = NavigatorTestFixture.task(project: "alpha")
        let other = NavigatorTestFixture.task("other", project: "beta")
        let model = NavigatorTestFixture.model([root, other])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.navigateNavigatorDependency(WorkDependencyRow(
            id: "other", title: other.name, status: other.status, kind: .task
        ))
        XCTAssertEqual(model.navigator.selectedRootID, "other")
        XCTAssertEqual(model.navigator.mode, .navigator)
    }

    func testOutOfScopeDependencyUsesRevealPath() {
        let root = NavigatorTestFixture.task(project: "alpha")
        let other = NavigatorTestFixture.task("other", project: "beta")
        let model = NavigatorTestFixture.model([root, other])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.changeNavigatorScope { model.selectedProjectFilterIDs = ["alpha"] }
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        model.navigateNavigatorDependency(WorkDependencyRow(
            id: "other", title: other.name, status: other.status, kind: .task
        ))
        XCTAssertEqual(model.navigator.selectedRootID, "other")
        XCTAssertTrue(model.selectedProjectFilterIDs.contains("beta"))
    }

    func testPerformEditBriefAndIsolatedVariantRenders() throws {
        let root = activityTask(section: "working", activities: [
            ["execution_id": "execution", "task_id": "root", "run_id": "run"],
        ])
        let model = NavigatorTestFixture.model([root])
        model.isConnected = true
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        model.performNavigatorCommand(NavigatorCommand(kind: "edit_brief", taskId: "root"))
        XCTAssertTrue(model.navigatorBriefIsEditing("root"))

        for (name, task) in [
            ("working", activityTask(section: "working", activities: [
                ["execution_id": "execution", "task_id": "root", "run_id": "run"],
            ])),
            ("backlog", activityTask(section: "backlog", waitKind: "waiting_dependency")),
            ("failure", activityTask(section: "backlog", waitKind: "failed", failed: true)),
            ("landed", activityTask(section: "landed", merge: true)),
        ] {
            let fixture = NavigatorTestFixture.model([task])
            fixture.isConnected = true
            fixture.selectWorkCard("root")
            fixture.setWorkViewMode(.navigator)
            let host = NSHostingView(rootView: WorkNavigatorView(model: fixture).frame(width: 1100, height: 700))
            host.frame = NSRect(x: 0, y: 0, width: 1100, height: 700)
            try capture(host, name: "navigator-activity-\(name)")
        }
    }

    func testClaimedSessionReleaseAndSwitchUnmount() {
        func session(_ id: String) -> TerminalPaneSession {
            TerminalPaneSession(id: id, role: .worker(slot: 1), launchSpec: TerminalLaunchSpec(
                fontSize: 12, workingDirectory: NSTemporaryDirectory(), initialInput: ""
            ))
        }
        func host(_ session: TerminalPaneSession, home: GhosttyTerminalAnchorView) -> GhosttyTerminalHostView {
            let host = GhosttyTerminalHostView(
                runtime: GhosttyRuntime.shared, session: session, launchSpec: session.launchSpec,
                paneMonitorEnabled: false, surfaceCreationEnabled: false
            )
            session.hostView = host
            session.homeAnchor = home
            home.install(host)
            return host
        }
        let first = session("first"), second = session("second")
        let firstHome = GhosttyTerminalAnchorView(), secondHome = GhosttyTerminalAnchorView()
        let firstHost = host(first, home: firstHome), secondHost = host(second, home: secondHome)
        let container = NSView()
        let coordinator = EmbeddedWorkerTerminalView.Coordinator(session: first)
        coordinator.update(session: first, in: container, isActive: true)
        XCTAssertTrue(firstHost.superview === container)
        coordinator.update(session: second, in: container, isActive: true)
        XCTAssertTrue(firstHost.superview === firstHome)
        XCTAssertTrue(secondHost.superview === container)
        EmbeddedWorkerTerminalView.dismantleNSView(container, coordinator: coordinator)
        XCTAssertTrue(secondHost.superview === secondHome)
        XCTAssertTrue(container.subviews.isEmpty)

        coordinator.update(session: second, in: container, isActive: true)
        second.markReleased()
        XCTAssertTrue(secondHost.isTornDown)
        XCTAssertNil(second.hostView)
        XCTAssertNil(secondHome.host)
        XCTAssertNil(secondHost.superview)
        let remountedHome = GhosttyTerminalAnchorView()
        XCTAssertTrue(remountedHome.reuseHost(of: first))
        first.homeAnchor = remountedHome
        XCTAssertTrue(firstHost.superview === remountedHome)
        XCTAssertNil(firstHome.host)
        GhosttyTerminalView.dismantleNSView(firstHome, coordinator: ())
        XCTAssertFalse(firstHost.isTornDown)
        firstHost.tearDown()
        XCTAssertNil(remountedHome.host)
        XCTAssertFalse(firstHome.reuseHost(of: first))
    }

    func testCurrentAskAdvancesWhileOldRevisionAndDraftRemain() {
        let first = NavigatorTestFixture.task("first", rootID: "root")
        let second = NavigatorTestFixture.task("second", rootID: "root")
        let root = activityTask(section: "working", revisions: [
            "open": 1, "total": 1, "current_task_id": "first",
        ])
        let model = NavigatorTestFixture.model([root, first])
        model.isConnected = true
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        model.beginNavigatorBriefEdit(first)
        model.setNavigatorBriefText(first.id, text: "Kept first draft")
        let updated = activityTask(section: "working", revisions: [
            "open": 1, "total": 2, "current_task_id": "second",
        ])
        NavigatorTestFixture.snapshot(model, [updated, first, second])
        XCTAssertEqual(model.navigator.askID, second.id)
        // The open draft keeps its ask in view when the current ask advances.
        XCTAssertEqual(model.navigatorDocumentTask(for: updated).id, first.id)
        XCTAssertEqual(model.navigatorBriefText(for: first), "Kept first draft")
        model.navigator.historicalDocumentByRoot[updated.id] = first.id
        model.setNavigatorDocument(.historical, rootID: updated.id)
        XCTAssertEqual(model.navigatorDocumentTask(for: updated).id, first.id)
        model.setNavigatorDocument(.currentAsk, rootID: updated.id)
        XCTAssertEqual(model.navigatorDocumentTask(for: updated).id, second.id)
    }

    func testActivityReorderRetainsDefaultAndPickerChangesItExplicitly() {
        let activities = [
            ["execution_id": "one", "task_id": "root", "run_id": "run-one"],
            ["execution_id": "two", "task_id": "revision", "run_id": "run-two"],
        ]
        let root = activityTask(section: "working", activities: activities)
        let model = NavigatorTestFixture.model([root])
        model.selectWorkCard(root.id)
        model.setWorkViewMode(.navigator)
        NavigatorTestFixture.snapshot(model, [activityTask(section: "working", activities: activities.reversed())])
        XCTAssertEqual(model.navigatorSelectedActivity(for: model.navigatorSelectedTask!)?.runId, "run-one")
        model.setNavigatorActivity(executionID: "two", rootID: root.id)
        XCTAssertEqual(model.navigatorSelectedActivity(for: model.navigatorSelectedTask!)?.runId, "run-two")
        XCTAssertTrue(WorkNavigatorActivity(context: context(model), workspace: nil).showsActivityPickerRow)
    }

    func testAskLabelUsesEngineRevisionSeqAndOrigin() {
        var ask = NavigatorTestFixture.task("third", rootID: "root")
        ask.revisionSeq = 3
        ask.createdVia = "ci-fix:abc"
        let inputs = WorkNavigatorBrief.revisionBadgeInputs(ask)
        XCTAssertEqual(inputs.seq, 3)
        XCTAssertEqual(inputs.origin, .ciFix)
        ask.createdVia = "manual"
        XCTAssertNil(WorkNavigatorBrief.revisionBadgeInputs(ask).origin)
    }

    func testRunningRevisionDefaultsToCurrentAskWithoutDraft() {
        let original = activityTask(section: "working", activities: [
            ["execution_id": "execution", "task_id": "root", "run_id": "run"],
        ])
        let model = NavigatorTestFixture.model([original])
        model.isConnected = true
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        XCTAssertEqual(model.navigatorDocument(for: "root"), .original)
        let revised = activityTask(section: "working", activities: [
            ["execution_id": "execution-2", "task_id": "revision", "run_id": "run-2"],
        ], revisions: ["open": 1, "total": 1, "current_task_id": "revision"])
        NavigatorTestFixture.snapshot(model, [revised, NavigatorTestFixture.task("revision", rootID: "root")])
        XCTAssertEqual(model.navigatorDocument(for: "root"), .currentAsk)
        XCTAssertEqual(model.navigatorDocumentTask(for: model.navigatorSelectedTask!).id, "revision")
        // The ask ends: the Brief falls back to the original under its own title.
        NavigatorTestFixture.snapshot(model, [original, NavigatorTestFixture.task("revision", rootID: "root")])
        model.setNavigatorDocument(.currentAsk, rootID: "root")
        model.reconcileNavigatorSelection()
        XCTAssertEqual(model.navigatorDocument(for: "root"), .original)
        XCTAssertEqual(model.navigatorDocumentTask(for: model.navigatorSelectedTask!).id, "root")
        let view = WorkNavigatorBrief(context: context(model))
        XCTAssertEqual(view.documentTitle, "Original brief")
    }

    func testEndedNoticeOnlyClaimsNewRevisionWhileWorking() {
        let working = activityTask(section: "working", activities: [
            ["execution_id": "execution", "task_id": "root", "run_id": "run"],
        ])
        let model = NavigatorTestFixture.model([working])
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        let landed = activityTask(section: "landed", merge: true)
        NavigatorTestFixture.snapshot(model, [landed])
        let view = WorkNavigatorActivity(context: context(model), workspace: nil)
        XCTAssertEqual(view.endedNotice, "Execution ended")
    }

    func testEndedNoticeWithAnotherLiveActivityAndRetainedRestingStates() {
        let working = activityTask(section: "working", activities: [
            ["execution_id": "execution", "task_id": "root", "run_id": "run"],
        ])
        let model = NavigatorTestFixture.model([working])
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        let moved = activityTask(section: "working", activities: [
            ["execution_id": "execution-2", "task_id": "revision", "run_id": "run-2"],
        ])
        NavigatorTestFixture.snapshot(model, [moved])
        var view = WorkNavigatorActivity(context: context(model), workspace: nil)
        XCTAssertEqual(view.endedNotice, "Execution ended; new revision available")

        for section in ["backlog", "backlog-failed"] {
            let resting = section == "backlog"
                ? activityTask(section: "backlog", waitKind: "waiting_dependency")
                : activityTask(section: "backlog", waitKind: "failed", failed: true)
            NavigatorTestFixture.snapshot(model, [resting])
            view = WorkNavigatorActivity(context: context(model), workspace: nil)
            XCTAssertEqual(view.endedNotice, "Execution ended", section)
        }
    }

    func testStartAndRetryRouteToRequestExecution() {
        for kind in ["start", "retry"] {
            let model = NavigatorTestFixture.model([activityTask(section: "backlog", waitKind: "waiting_dependency")])
            XCTAssertFalse(model.engine.hasUnsentAction(kind: "request_execution"))
            model.performNavigatorCommand(NavigatorCommand(kind: kind, taskId: "root"))
            XCTAssertTrue(model.engine.hasUnsentAction(kind: "request_execution"), kind)
        }
    }

    func testSlotPresentationRequiresHostedSessionAndFollowsEndedAndFailure() {
        let workspace = WorkersWorkspaceModel()
        _ = workspace.attachWorkerPane(EngineAttachRequest(
            runId: "run", slotId: 1, sessionName: "boss-1-run",
            tmuxSocketPath: "/state/boss/tmux.sock", summary: nil, taskTitle: nil
        ))
        let session = workspace.viewer(forRunId: "run")?.session
        XCTAssertNotNil(session)
        XCTAssertNil(session?.hostView, "A non-PTY attach has no host view")
        typealias Slot = WorkNavigatorTerminalSlot
        let hosted = session?.hostView != nil
        XCTAssertEqual(Slot.presentation(hasHostedSession: hosted, ended: false, failure: nil), .unavailable)
        XCTAssertEqual(Slot.presentation(hasHostedSession: hosted, ended: true, failure: "x"), .ended)
        XCTAssertEqual(Slot.presentation(hasHostedSession: false, ended: false, failure: "busy"), .attachFailure("busy"))
        XCTAssertEqual(Slot.presentation(hasHostedSession: true, ended: true, failure: "busy"), .terminal)
    }

    func testAttachRequestConditions() {
        typealias Slot = WorkNavigatorTerminalSlot
        XCTAssertTrue(Slot.shouldRequestAttach(isActive: true, isLive: true, ended: false, hasSession: false))
        XCTAssertFalse(Slot.shouldRequestAttach(isActive: false, isLive: true, ended: false, hasSession: false))
        XCTAssertFalse(Slot.shouldRequestAttach(isActive: true, isLive: false, ended: false, hasSession: false))
        XCTAssertFalse(Slot.shouldRequestAttach(isActive: true, isLive: true, ended: true, hasSession: false))
        XCTAssertFalse(Slot.shouldRequestAttach(isActive: true, isLive: true, ended: false, hasSession: true))
    }

    func testEndingAttachRequestAllowsASecondRequestWithoutClearingOthers() {
        let workspace = WorkersWorkspaceModel()
        var requested: [String] = []
        workspace.onViewerAttachRequested = { requested.append($0) }
        workspace.requestViewerAttach(forRuns: ["run-a", "run-b"])
        workspace.requestViewerAttach(forRuns: ["run-a"])
        XCTAssertEqual(requested, ["run-a", "run-b"])
        workspace.endViewerAttachRequest(forRun: "run-a")
        workspace.requestViewerAttach(forRuns: ["run-a", "run-b"])
        XCTAssertEqual(requested, ["run-a", "run-b", "run-a"])
    }

    func testRemoteRunIsIdentifiedForRemotePane() {
        let root = activityTask(section: "working", activities: [
            ["execution_id": "execution", "task_id": "root", "run_id": "run"],
        ])
        let model = NavigatorTestFixture.model([root])
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        let remote = liveState(runId: "run", activity: .working, hostId: "spoke-1")
        model.applyEventForTest(.workerLiveStatesList(states: [remote]))
        let view = WorkNavigatorActivity(context: context(model), workspace: nil)
        let activity = model.navigatorSelectedActivity(for: root)!
        XCTAssertEqual(view.remoteHost(for: activity), "spoke-1")
        XCTAssertTrue(view.hasAgentPane)
        model.applyEventForTest(.workerLiveStatesList(states: [liveState(runId: "run", activity: .working)]))
        XCTAssertNil(WorkNavigatorActivity(context: context(model), workspace: nil).remoteHost(for: activity))
    }

    func testDraftOnDefaultCurrentAskSurvivesClearAndAdvance() {
        let revised = activityTask(section: "working", activities: [
            ["execution_id": "e", "task_id": "revision", "run_id": "run"],
        ], revisions: ["open": 1, "total": 1, "current_task_id": "revision"])
        let ask = NavigatorTestFixture.task("revision", rootID: "root")
        let model = NavigatorTestFixture.model([revised, ask])
        model.isConnected = true
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        XCTAssertNil(model.navigator.documentByRoot["root"])
        XCTAssertEqual(model.navigatorDocument(for: "root"), .currentAsk)
        model.beginNavigatorBriefEdit(ask)
        model.setNavigatorBriefText(ask.id, text: "ask draft")

        NavigatorTestFixture.snapshot(model, [activityTask(section: "working"), ask])
        model.reconcileNavigatorSelection()
        XCTAssertEqual(model.navigatorDocument(for: "root"), .historical)
        XCTAssertEqual(model.navigatorDocumentTask(for: model.navigatorSelectedTask!).id, "revision")
        XCTAssertEqual(model.navigatorBriefText(for: ask), "ask draft")
        XCTAssertEqual(WorkNavigatorBrief(context: context(model)).documentTitle, "Historical ask")
    }

    func testDraftOnDefaultCurrentAskSurvivesAdvanceToAnotherRevision() {
        let first = activityTask(section: "working", revisions: ["open": 1, "total": 1, "current_task_id": "revision"])
        let ask = NavigatorTestFixture.task("revision", rootID: "root")
        let model = NavigatorTestFixture.model([first, ask])
        model.isConnected = true
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        model.beginNavigatorBriefEdit(ask)
        model.setNavigatorBriefText(ask.id, text: "first draft")
        let second = NavigatorTestFixture.task("revision-2", rootID: "root")
        NavigatorTestFixture.snapshot(model, [
            activityTask(section: "working", revisions: ["open": 1, "total": 2, "current_task_id": "revision-2"]),
            ask, second,
        ])
        model.reconcileNavigatorSelection()
        XCTAssertEqual(model.navigator.askID, "revision-2")
        XCTAssertEqual(model.navigatorDocumentTask(for: model.navigatorSelectedTask!).id, "revision")
        XCTAssertEqual(model.navigatorBriefText(for: ask), "first draft")
    }

    func testFailingTaskPrefersFailedRevisionOverStickyRootActivity() {
        let root = activityTask(section: "working", activities: [
            ["execution_id": "execution", "task_id": "root", "run_id": "run"],
        ])
        let model = NavigatorTestFixture.model([root])
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        XCTAssertEqual(model.navigatorSelectedActivity(for: root)?.taskId, "root")
        var failedRoot = activityTask(section: "backlog", waitKind: "failed", failed: true,
                                      revisions: ["open": 1, "total": 1, "current_task_id": "revision"])
        failedRoot.dispatchFailedReason = nil
        var revision = NavigatorTestFixture.task("revision", rootID: "root")
        revision.dispatchFailedReason = "cube_workspace_lease_failed"
        NavigatorTestFixture.snapshot(model, [failedRoot, revision])
        let view = WorkNavigatorActivity(context: context(model), workspace: nil)
        XCTAssertEqual(model.navigatorSelectedActivity(for: model.navigatorSelectedTask!)?.taskId, "root")
        XCTAssertEqual(view.failingTask.id, "revision")
    }

    func testEditBriefRevealsTheDocumentItEdits() {
        let root = activityTask(section: "working", activities: [
            ["execution_id": "execution", "task_id": "root", "run_id": "run"],
        ], revisions: ["open": 1, "total": 1, "current_task_id": "revision"])
        let model = NavigatorTestFixture.model([root, NavigatorTestFixture.task("revision", rootID: "root")])
        model.isConnected = true
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        XCTAssertEqual(model.navigatorDocument(for: "root"), .currentAsk)
        model.performNavigatorCommand(NavigatorCommand(kind: "edit_brief", taskId: "root"))
        XCTAssertEqual(model.navigatorDocument(for: "root"), .original)
        XCTAssertTrue(model.navigatorBriefIsEditing("root"))
    }

    func testParkCommandRequestsPark() {
        let root = activityTask(section: "backlog", waitKind: "waiting_dependency")
        let model = NavigatorTestFixture.model([root])
        model.performNavigatorCommand(NavigatorCommand(kind: "park", taskId: "root"))
        XCTAssertEqual(model.pendingParkRequest?.id, "root")
    }

    func testLandedBriefCannotEditOrSave() {
        let root = activityTask(section: "landed", merge: true)
        let model = NavigatorTestFixture.model([root])
        model.isConnected = true
        model.engine.simulateConnectedForTesting = true
        XCTAssertFalse(model.navigatorBriefCanEdit(root))
        model.beginNavigatorBriefEdit(root)
        XCTAssertFalse(model.navigatorBriefIsEditing(root.id))
        XCTAssertFalse(model.saveNavigatorBrief(root))
    }

    func testHistoryConsumerTracksFamilyChangesAndUnmount() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task()])
        let consumer = WorkExecutionsConsumer()
        consumer.update(taskIDs: ["root"], model: model)
        consumer.update(taskIDs: ["root", "revision"], model: model)
        XCTAssertEqual(consumer.registered, ["root", "revision"])
        XCTAssertNotNil(model.executionsConsumerTokensByTaskID["revision"])
        consumer.update(taskIDs: ["revision"], model: model)
        XCTAssertNil(model.executionsConsumerTokensByTaskID["root"])
        consumer.update(taskIDs: [], model: model)
        XCTAssertTrue(model.executionsConsumerTokensByTaskID.isEmpty)
    }

    private func activityTask(
        id: String = "root",
        section: String,
        waitKind: String? = nil,
        activities: [[String: Any]] = [],
        failed: Bool = false,
        merge: Bool = false,
        revisions: [String: Any] = ["open": 0, "total": 0]
    ) -> WorkTask {
        var task = NavigatorTestFixture.task(id, section: section)
        var reason: [String: Any] = ["type": waitKind == nil ? "active" : "wait", "activity": "revising"]
        if let waitKind { reason["wait"] = ["kind": waitKind] }
        var root: [String: Any] = [
            "section": section, "sort_key": "0", "default_detail": "activity",
            "reason": reason, "reason_label": "Reason",
            "revisions": revisions, "activities": activities,
            "commands": [["kind": "start", "task_id": id], ["kind": "edit_brief", "task_id": id]],
        ]
        if merge {
            root["merge_evidence"] = [
                "pr_url": "https://github.com/spinyfin/mono/pull/3629",
                "observed_at": "2026-11-14T10:32:00Z",
                "merged_at": "2026-11-14T10:32:00Z",
            ]
        }
        task.navigator = NavigatorProjection.parse(["version": 1, "root_task_id": id, "root": root])
        if failed { task.dispatchFailedReason = "cube_workspace_lease_failed" }
        return task
    }

    private func context(_ model: ChatViewModel) -> NavigatorDetailContext {
        NavigatorDetailContext(
            model: model,
            root: model.navigatorSelectedTask!,
            ask: model.navigator.askID.flatMap { model.task(withID: $0) },
            isReadOnly: !model.isConnected
        )
    }

    private func liveState(runId: String, slotId: Int = 3, activity: WorkerActivity, hostId: String? = nil) -> WorkerLiveState {
        WorkerLiveState(
            name: "Riker", slotId: slotId, runId: runId, model: "claude-opus", shellPid: 0,
            lastEventAt: nil, currentTool: nil, lastToolEndedAt: nil, activity: activity,
            liveStatus: nil, liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true, hostId: hostId
        )
    }

    private func capture<V: View>(_ host: NSHostingView<V>, name: String) throws {
        host.appearance = NSAppearance(named: .aqua)
        for _ in 0..<3 {
            host.layoutSubtreeIfNeeded()
            RunLoop.current.run(until: Date().addingTimeInterval(0.02))
        }
        host.layoutSubtreeIfNeeded()
        let rep = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
        host.cacheDisplay(in: host.bounds, to: rep)
        let data = try XCTUnwrap(rep.representation(using: .png, properties: [:]))
        XCTAssertGreaterThan(data.count, 100)
        if let output = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"] {
            try data.write(to: URL(fileURLWithPath: output).appendingPathComponent(name + ".png"))
        }
    }
}
