import AppKit
import XCTest
@testable import Boss

@MainActor
final class NavigatorLifecycleTests: XCTestCase {
    private func activity(_ execution: String = "attempt-a", task: String = "root") -> NavigatorActivity {
        NavigatorActivity(executionId: execution, taskId: task, runId: execution)
    }

    private func root(current: String? = nil, activities: [NavigatorActivity] = []) -> WorkTask {
        var task = NavigatorTestFixture.task()
        var revisions: [String: Any] = ["open": 0, "total": 0]
        if let current { revisions["current_task_id"] = current }
        task.navigator = NavigatorProjection.parse([
            "version": 1, "root_task_id": "root", "root": [
                "section": "working", "sort_key": "0", "default_detail": "activity", "reason_label": "Revising",
                "reason": ["type": "active", "activity": "revising"],
                "revisions": revisions,
                "activities": activities.map { ["execution_id": $0.executionId, "task_id": $0.taskId, "run_id": $0.runId!] },
            ],
        ])
        return task
    }

    private func view(_ model: ChatViewModel, workspace: WorkersWorkspaceModel) -> WorkNavigatorActivity {
        WorkNavigatorActivity(context: NavigatorDetailContext(
            model: model, root: model.navigatorSelectedTask!, ask: nil, isReadOnly: false
        ), workspace: workspace)
    }

    func testActivityActivationUsesNavigationModeWorkModeAndTab() {
        let model = NavigatorTestFixture.model([root()])
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        model.navigationMode = .work
        model.navigator.tab = .activity
        let workspace = WorkersWorkspaceModel()
        XCTAssertTrue(view(model, workspace: workspace).isSlotActive)
        model.navigator.tab = .history
        XCTAssertFalse(view(model, workspace: workspace).isSlotActive)
        model.navigator.tab = .activity
        model.setWorkViewMode(.kanban)
        XCTAssertFalse(view(model, workspace: workspace).isSlotActive)
        model.setWorkViewMode(.navigator)
        model.navigationMode = .agents
        XCTAssertFalse(view(model, workspace: workspace).isSlotActive)
    }

    func testSlotResolvesDistinctViewersAndHostGate() throws {
        let workspace = WorkersWorkspaceModel()
        for (index, run) in ["attempt-a", "attempt-b"].enumerated() {
            _ = workspace.attachWorkerPane(EngineAttachRequest(
                runId: run, slotId: index + 1, sessionName: "boss-test-\(run)",
                tmuxSocketPath: "/state/boss/tmux.sock", summary: nil, taskTitle: nil
            ))
        }
        func slot(_ run: String, ended: Bool = false) -> WorkNavigatorTerminalSlot {
            WorkNavigatorTerminalSlot(selected: activity(run), ended: ended, isLive: true,
                                      isActive: true, workspace: workspace)
        }
        let first = slot("attempt-a"), second = slot("attempt-b")
        let session = try XCTUnwrap(first.session)
        XCTAssertFalse(session === second.session)
        XCTAssertEqual(first.presentation, .unavailable)
        XCTAssertEqual(slot("attempt-a", ended: true).presentation, .ended)
        let host = GhosttyTerminalHostView(
            runtime: GhosttyRuntime.shared, session: session, launchSpec: session.launchSpec,
            paneMonitorEnabled: false, surfaceCreationEnabled: false
        )
        session.hostView = host
        XCTAssertEqual(first.presentation, .terminal)
        XCTAssertEqual(second.presentation, .unavailable)
        var requests: [String] = []
        workspace.onViewerAttachRequested = { requests.append($0) }
        first.updateAttachRequest(owner: "hosted-slot")
        XCTAssertTrue(requests.isEmpty, "A hosted slot never requests an attach")
        host.tearDown()
    }

    func testFailedAttachHideShowAndSameRunOwnership() {
        let workspace = WorkersWorkspaceModel()
        var requests: [String] = []
        workspace.onViewerAttachRequested = { requests.append($0) }
        // A malformed attach supplies the production failure state without spawning a PTY.
        _ = workspace.attachWorkerPane(EngineAttachRequest(
            runId: "attempt-a", slotId: 1, sessionName: "", tmuxSocketPath: "",
            summary: nil, taskTitle: nil
        ))
        let active = WorkNavigatorTerminalSlot(selected: activity(), ended: false, isLive: true,
                                               isActive: true, workspace: workspace)
        active.updateAttachRequest(owner: "slot")
        active.updateAttachRequest(owner: "slot")
        XCTAssertEqual(requests, ["attempt-a"])
        if let failure = workspace.viewerAttachFailuresByRunId["attempt-a"] {
            XCTAssertEqual(active.presentation, .attachFailure(failure))
        } else { XCTFail("Expected attach refusal") }
        var hidden = active
        hidden.isActive = false
        hidden.updateAttachRequest(owner: "slot")
        active.updateAttachRequest(owner: "slot")
        XCTAssertEqual(requests, ["attempt-a", "attempt-a"])

        workspace.requestViewerAttach(forRuns: ["shared"], owner: "agents")
        workspace.requestViewerAttach(forRuns: ["shared"], owner: "navigator")
        workspace.resetViewerAttachRequests(owner: "navigator")
        workspace.requestViewerAttach(forRuns: ["shared"], owner: "agents")
        XCTAssertEqual(requests.filter { $0 == "shared" }.count, 1)
        let neverRequested = WorkNavigatorTerminalSlot(selected: activity("shared"), ended: false, isLive: false,
                                                       isActive: false, workspace: workspace)
        neverRequested.updateAttachRequest(owner: "never-requested-slot")
        workspace.endViewerAttachRequest(forRun: "shared", owner: "never-requested")
        workspace.requestViewerAttach(forRuns: ["shared"], owner: "agents")
        XCTAssertEqual(requests.filter { $0 == "shared" }.count, 1)
    }

    func testHiddenHistoryAndActiveFamilyTransitions() {
        let model = NavigatorTestFixture.model([root()])
        let consumer = WorkExecutionsConsumer()
        var list = WorkExecutionsList(model: model, taskIDs: ["root"], transcriptTaskID: "root", isActive: false)
        list.updateConsumer(consumer)
        XCTAssertTrue(model.executionsConsumerTokensByTaskID.isEmpty)
        list.isActive = true
        list.updateConsumer(consumer)
        XCTAssertEqual(consumer.registered, ["root"])
        list = WorkExecutionsList(model: model, taskIDs: ["root", "revision"], transcriptTaskID: "root")
        list.updateConsumer(consumer)
        XCTAssertEqual(consumer.registered, ["root", "revision"])
        list.isActive = false
        list.updateConsumer(consumer)
        XCTAssertTrue(model.executionsConsumerTokensByTaskID.isEmpty)
    }

    func testHistoryPrefetchSurvivesTabSwitchAndSharedConsumer() {
        let model = NavigatorTestFixture.model([root()])
        model.selectWorkCard("root")
        model.navigator.tab = .activity
        var requests = 0
        model.engine.outboundRecorder = { payload in
            if payload["type"] as? String == "list_executions" { requests += 1 }
        }
        let history = WorkNavigatorHistory(context: NavigatorDetailContext(
            model: model, root: model.navigatorSelectedTask!, ask: nil, isReadOnly: false
        ))
        let consumer = WorkExecutionsConsumer()
        history.executionsList.updateConsumer(consumer)
        XCTAssertEqual(requests, 1, "Prefetch before History is selected")
        let viewer = UUID()
        model.beginExecutionsConsumer(taskID: "root", token: viewer)
        XCTAssertEqual(requests, 1, "Share the pending request")
        model.applyEventForTest(.executionsList(taskId: "root", executions: []))
        for tab in [NavigatorDetailSlot.history, .activity, .history] {
            model.navigator.tab = tab
            history.executionsList.updateConsumer(consumer)
            XCTAssertNotNil(model.executionsByTaskID["root"])
        }
        XCTAssertEqual(requests, 1, "Switching tabs must retain the loaded result")
        model.endExecutionsConsumer(taskID: "root", token: viewer)
        consumer.update(taskIDs: [], model: model)
        XCTAssertNil(model.executionsByTaskID["root"], "Closing the detail releases its cache")
    }

    func testRejectedStartAndRetryNeverQueueExecution() {
        for kind in ["start", "retry"] {
            let model = NavigatorTestFixture.model([root()])
            for index in 0..<EngineClient.maxUnsentActions {
                model.engine.sendUpdateWorkItem(id: "queued-\(index)", patch: ["description": "queued"])
            }
            XCTAssertFalse(model.canSubmitUserActions())
            model.performNavigatorCommand(NavigatorCommand(kind: kind, taskId: "root"))
            XCTAssertFalse(model.engine.hasUnsentAction(kind: "request_execution"))
        }
    }

    func testRemoteOwnershipSwitchesAndHiddenRemoval() {
        let model = RemotePaneViewerModel()
        var subscribed: [String] = [], unsubscribed: [String] = []
        model.subscribe = { subscribed += $0 }
        model.unsubscribe = { unsubscribed += $0 }
        for (outgoing, incoming) in [("work", "agents"), ("agents", "work")] {
            model.watch("a", owner: outgoing)
            model.watch("b", owner: incoming)
            model.stopWatching(owner: outgoing)
            model.stopWatching(owner: outgoing)
            XCTAssertEqual(model.watching, ["b"])
            XCTAssertFalse(unsubscribed.suffix(1).contains(RemotePaneViewerModel.topic(runId: "b")))
            model.stopWatching(owner: incoming)
        }
        XCTAssertEqual(subscribed.count, 4)
        XCTAssertEqual(unsubscribed.count, 4)
    }

    func testEndedRemoteSelectionKeepsTranscriptWhileAnotherActivityIsLive() {
        let selected = activity()
        let model = NavigatorTestFixture.model([root(activities: [selected])])
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        NavigatorTestFixture.snapshot(model, [root(activities: [activity("attempt-b")])])
        let pane = WorkNavigatorRemotePane(model: model.remotePaneViewer, selected: selected,
                                           host: "spoke", ended: true, isActive: true)
        XCTAssertNil(pane.watchTarget)
        XCTAssertEqual(pane.transcript.taskId, "root")
        XCTAssertEqual(pane.transcript.preselectExecutionId, "attempt-a")
        pane.updateWatch(owner: "work")
        XCTAssertTrue(model.remotePaneViewer.watching.isEmpty)
        let active = WorkNavigatorRemotePane(model: model.remotePaneViewer, selected: activity("attempt-b"),
                                             host: "spoke", ended: false, isActive: true)
        active.updateWatch(owner: "work")
        XCTAssertEqual(model.remotePaneViewer.watching, ["attempt-b"])
        let hidden = WorkNavigatorRemotePane(model: model.remotePaneViewer, selected: activity("attempt-b"),
                                             host: "spoke", ended: false, isActive: false)
        hidden.updateWatch(owner: "work")
        XCTAssertTrue(model.remotePaneViewer.watching.isEmpty)
    }

    func testDraftRetirementWhileAnotherRootSelectedAndReleaseOnCancelOrSave() {
        for advances in [false, true] {
            for saves in [false, true] {
                let first = NavigatorTestFixture.task("first", rootID: "root")
                let second = NavigatorTestFixture.task("second", rootID: "root")
                let other = NavigatorTestFixture.task("other")
                let model = NavigatorTestFixture.model([root(current: "first"), first, other])
                model.isConnected = true
                model.selectWorkCard("root")
                model.setWorkViewMode(.navigator)
                model.beginNavigatorBriefEdit(first)
                model.setNavigatorBriefText(first.id, text: "kept draft")
                model.selectNavigatorRoot("other")
                let updated = root(current: advances ? "second" : nil)
                NavigatorTestFixture.snapshot(model, [updated, first, second, other])
                model.selectNavigatorRoot("root")
                XCTAssertEqual(model.navigatorDocumentTask(for: updated).id, "first")
                XCTAssertEqual(model.navigatorBriefText(for: first), "kept draft")
                if saves { XCTAssertTrue(model.saveNavigatorBrief(first)) }
                else { model.cancelNavigatorBriefEdit(first) }
                XCTAssertEqual(model.navigatorDocument(for: "root"), advances ? .currentAsk : .original)
            }
        }
    }

    func testFailureTranscriptUsesRuntimeAttemptAndFailedRevisionWithoutDispatchMarker() {
        let model = NavigatorTestFixture.model([root(activities: [activity()])])
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        func runtime(_ task: String, execution: String, status: String) -> WorkTaskRuntime {
            WorkTaskRuntime(workItemID: task, executionStatus: status, runStatus: nil,
                            executionID: execution, dispatchWaitReason: nil, dispatchWaitSince: nil)
        }
        model.taskRuntimesByID["root"] = runtime("root", execution: "attempt-b", status: "failed")
        let workspace = WorkersWorkspaceModel()
        XCTAssertEqual(view(model, workspace: workspace).failureTranscript.preselectExecutionId, "attempt-b")
        let revision = NavigatorTestFixture.task("revision", rootID: "root")
        NavigatorTestFixture.snapshot(model, [root(current: "revision"), revision])
        model.taskRuntimesByID["revision"] = runtime("revision", execution: "revision-run", status: "orphaned")
        XCTAssertEqual(view(model, workspace: workspace).failureTranscript.taskId, "revision")
        XCTAssertEqual(view(model, workspace: workspace).failureTranscript.preselectExecutionId, "revision-run")
    }
}
