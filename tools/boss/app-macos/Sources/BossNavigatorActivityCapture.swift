import AppKit
import Foundation
import SwiftUI
import UpdateCore

/// Isolated Activity-slot fixtures: `--capture-reveal-state=navigator-activity-*
/// --capture-to`. States: working, backlog, failure, landed, revision.
///
/// Isolated captures do not mount Agents Ghostty surfaces, so the working
/// pane shows "Agent pane not available yet" rather than a live PTY.
@MainActor
enum BossNavigatorActivityCapture {
    private static var window: NSWindow?

    static func start(path: String, state: String, updateModel: UpdateModel) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let product = WorkProduct(
            id: "nav-activity", name: "Navigator activity", slug: "nav-activity", description: "",
            repoRemoteURL: nil, status: "active", createdAt: "2026-11-01T00:00:00Z",
            updatedAt: "2026-11-01T00:00:00Z"
        )
        let project = WorkProject(
            id: "alpha", productID: product.id, name: "Keyboard routing", slug: "alpha",
            description: "", goal: "", status: "active", priority: "medium",
            createdAt: "2026-11-01T00:00:00Z", updatedAt: "2026-11-01T00:00:00Z"
        )
        model.products = [product]
        model.selectWorkProduct(product.id)
        model.isConnected = true
        model.setNavigationMode(.work, refreshWorkOnEntry: false)

        let kind = String(state.dropFirst("navigator-activity-".count))
        let tasks: [WorkTask]
        var runtimes: [WorkTaskRuntime] = []
        var dependencies: [WorkItemDependency] = []
        var live: [WorkerLiveState] = []
        switch kind {
        case "working":
            tasks = [rootTask(product: product, section: "working", wait: nil,
                              reasonType: "active", activity: "revising",
                              reasonLabel: "Revising",
                              activities: [["execution_id": "execution", "task_id": "root", "run_id": "run"]],
                              commands: [["kind": "park", "task_id": "root"],
                                         ["kind": "edit_brief", "task_id": "root"]])]
            live = [liveState(runId: "run", activity: .working)]
        case "backlog":
            let blocked = prereqTask(product: product)
            var root = rootTask(product: product, section: "backlog",
                                wait: ["kind": "waiting_dependency", "blocker": "prereq",
                                       "label": "waiting on shared selection"],
                                reasonType: "wait", reasonLabel: "Waiting on a dependency",
                                commands: [["kind": "start", "task_id": "root"],
                                           ["kind": "park", "task_id": "root"],
                                           ["kind": "edit_brief", "task_id": "root"]])
            root.status = "blocked"
            tasks = [root, blocked]
            dependencies = [WorkItemDependency(dependentID: "root", prerequisiteID: "prereq", relation: "blocks")]
        case "failure":
            var root = rootTask(product: product, section: "backlog",
                                wait: ["kind": "failed"],
                                reasonType: "wait", reasonLabel: "Last execution failed",
                                commands: [["kind": "retry", "task_id": "root"],
                                           ["kind": "edit_brief", "task_id": "root"]])
            root.dispatchFailedReason = "cube_workspace_lease_failed"
            root.dispatchFailedError = "Could not attach a workspace for this run."
            tasks = [root]
            runtimes = [WorkTaskRuntime(
                workItemID: "root", executionStatus: "failed", runStatus: "failed",
                executionID: "execution", dispatchRetryAt: nil, dispatchWaitReason: "failed",
                dispatchWaitSince: nil, dispatchWaitLabel: "Last execution failed",
                dispatchFailures: [DispatchFailureAttempt(
                    errorText: "Driver has made no observable progress while awaiting model output",
                    happenedAt: "1899996400"
                )]
            )]
        case "landed":
            var root = rootTask(product: product, section: "landed", wait: nil,
                                reasonType: "merged", reasonLabel: "Merged",
                                merge: ["pr_url": "https://github.com/spinyfin/mono/pull/3629",
                                        "observed_at": "2026-11-14T10:32:00Z",
                                        "merged_at": "2026-11-14T10:32:00Z"])
            root.status = "done"
            tasks = [root]
        case "revision":
            let original = rootTask(product: product, section: "working", wait: nil,
                                    reasonType: "active", activity: "implementing",
                                    reasonLabel: "Implementing",
                                    activities: [["execution_id": "execution-1", "task_id": "root",
                                                  "run_id": "run-1"]])
            model.applyEventForTest(.workTree(
                product: product, projects: [project], tasks: [original], chores: [],
                taskRuntimes: [], dependencies: [], ideas: []
            ))
            model.selectWorkCard("root")
            model.setWorkViewMode(.navigator)
            var revised = rootTask(product: product, section: "working", wait: nil,
                                   reasonType: "active", activity: "revising",
                                   reasonLabel: "Revising",
                                   revisions: ["open": 1, "total": 1, "current_task_id": "revision"],
                                   activities: [["execution_id": "execution-2", "task_id": "revision",
                                                 "run_id": "run-2"]],
                                   commands: [["kind": "park", "task_id": "root"],
                                              ["kind": "edit_brief", "task_id": "revision"]])
            revised.description = "Original navigator brief for the keyboard routing work."
            var ask = NavigatorTestFixtureAsk.revision(product: product)
            ask.description = "Repair keyboard routing after the review findings."
            tasks = [revised, ask]
            live = [liveState(runId: "run-2", activity: .working, workItemId: "revision")]
        default:
            fputs("unknown navigator activity capture state: \(state)\n", stderr)
            exit(1)
        }

        if kind != "revision" {
            model.applyEventForTest(.workTree(
                product: product, projects: [project], tasks: tasks, chores: [],
                taskRuntimes: runtimes, dependencies: dependencies, ideas: []
            ))
            model.selectWorkCard("root")
            model.setWorkViewMode(.navigator)
        } else {
            model.applyEventForTest(.workTree(
                product: product, projects: [project], tasks: tasks, chores: [],
                taskRuntimes: [], dependencies: [], ideas: []
            ))
        }
        if !live.isEmpty {
            model.applyEventForTest(.workerLiveStatesList(states: live, doingSections: ["root": .coding]))
        }
        model.navigator.tab = .activity

        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1280, height: 820),
            styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false
        )
        captureWindow.title = "Boss [agent capture]"
        captureWindow.contentView = NSHostingView(rootView: ContentView()
            .environmentObject(model).environmentObject(updateModel))
        window = captureWindow
        captureWindow.contentView?.layoutSubtreeIfNeeded()
        DispatchQueue.main.asyncAfter(deadline: .now() + max(BossCaptureArgs.shared.captureAfter, 1.0)) {
            captureWindow.contentView?.layoutSubtreeIfNeeded()
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                NSApp.terminate(nil)
            } catch {
                fputs("navigator activity capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }

    private static func rootTask(
        product: WorkProduct,
        section: String,
        wait: [String: Any]?,
        reasonType: String,
        activity: String? = nil,
        reasonLabel: String,
        revisions: [String: Any] = ["open": 0, "total": 0],
        activities: [[String: Any]] = [],
        commands: [[String: Any]] = [],
        merge: [String: Any]? = nil
    ) -> WorkTask {
        var task = WorkTask(
            id: "root", productID: product.id, projectID: "alpha", kind: "task",
            name: "Improve keyboard routing",
            description: "Implement dense navigator rows and keep the original brief editable.",
            status: "active", priority: "medium", ordinal: 1,
            prURL: "https://github.com/spinyfin/mono/pull/3629",
            deletedAt: nil, createdAt: "2026-11-01T00:00:00Z", updatedAt: "2026-11-14T00:00:00Z"
        )
        task.shortID = 42
        var reason: [String: Any] = ["type": reasonType]
        if let activity { reason["activity"] = activity }
        if let wait { reason["wait"] = wait }
        var root: [String: Any] = [
            "section": section, "sort_key": "0", "default_detail": "activity",
            "reason": reason, "reason_label": reasonLabel, "revisions": revisions,
            "activities": activities, "commands": commands,
        ]
        if let merge { root["merge_evidence"] = merge }
        task.navigator = NavigatorProjection.parse([
            "version": 1, "root_task_id": "root", "root": root,
        ])
        return task
    }

    private static func prereqTask(product: WorkProduct) -> WorkTask {
        var task = WorkTask(
            id: "prereq", productID: product.id, projectID: "alpha", kind: "task",
            name: "Land shared selection", description: "Shared root selection for both views.",
            status: "todo", priority: "medium", ordinal: 0, prURL: nil, deletedAt: nil,
            createdAt: "2026-10-20T00:00:00Z", updatedAt: "2026-10-20T00:00:00Z"
        )
        task.navigator = NavigatorProjection.parse([
            "version": 1, "root_task_id": "prereq", "root": [
                "section": "backlog", "sort_key": "1", "default_detail": "activity",
                "reason": ["type": "wait"], "reason_label": "Queued",
                "revisions": ["open": 0, "total": 0],
            ],
        ])
        return task
    }

    private static func liveState(runId: String, activity: WorkerActivity, workItemId: String = "root") -> WorkerLiveState {
        WorkerLiveState(
            name: "Riker", slotId: 3, runId: runId, model: "claude-opus", shellPid: 0,
            lastEventAt: "2026-11-14T10:00:00Z", currentTool: nil, lastToolEndedAt: nil,
            activity: activity, liveStatus: "Editing navigator rows", liveStatusAt: "2026-11-14T10:00:00Z",
            recoveryStatus: nil, tmuxHosted: true, agentType: "coding",
            workItemId: workItemId, workItemName: "Improve keyboard routing", workItemShortId: 42,
            productId: "nav-activity", prURL: "https://github.com/spinyfin/mono/pull/3629"
        )
    }
}

private enum NavigatorTestFixtureAsk {
    static func revision(product: WorkProduct) -> WorkTask {
        var task = WorkTask(
            id: "revision", productID: product.id, projectID: "alpha", kind: "revision",
            name: "Repair keyboard routing",
            description: "Repair keyboard routing after the review findings.",
            status: "active", priority: "medium", ordinal: nil,
            prURL: "https://github.com/spinyfin/mono/pull/3629",
            deletedAt: nil, createdAt: "2026-11-14T12:00:00Z", updatedAt: "2026-11-14T12:00:00Z"
        )
        task.navigator = NavigatorProjection.parse(["version": 1, "root_task_id": "root"])
        return task
    }
}
