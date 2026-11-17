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
        model.reloadNavigatorBrief(root)
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
        model.setNavigatorActivity(executionID: "execution", rootID: "root")
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
        XCTAssertTrue(view.selectedExecutionEnded)
        XCTAssertFalse(view.isLiveSelected)
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

    private func liveState(runId: String, slotId: Int = 3, activity: WorkerActivity) -> WorkerLiveState {
        WorkerLiveState(
            name: "Riker", slotId: slotId, runId: runId, model: "claude-opus", shellPid: 0,
            lastEventAt: nil, currentTool: nil, lastToolEndedAt: nil, activity: activity,
            liveStatus: nil, liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true
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
