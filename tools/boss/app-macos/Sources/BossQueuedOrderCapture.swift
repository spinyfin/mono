import AppKit
import Foundation
import SwiftUI
import UpdateCore

/// Quiet, isolated fixture: --capture-reveal-state=queued-order --capture-to.
/// Hosts the real ContentView on a Doing column holding a coding card, two
/// operator decisions, and a queue in engine dispatch order (the oldest task
/// is deliberately not first), plus cards the dispatcher is not taking.
@MainActor
enum BossQueuedOrderCapture {
    private static var window: NSWindow?

    static func start(path: String, updateModel: UpdateModel) {
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let product = WorkProduct(
            id: "queue-fixture", name: "Queue order", slug: "queue-fixture", description: "",
            repoRemoteURL: nil, status: "active", createdAt: "2026-10-01T00:00:00Z", updatedAt: "2026-10-01T00:00:00Z"
        )
        model.products = [product]
        model.selectWorkProduct(product.id)
        model.setNavigationMode(.work)

        func card(_ id: String, _ name: String, age: Int, shortID: Int) -> WorkTask {
            let stamp = "2026-10-\(String(format: "%02d", age))T00:00:00Z"
            var task = WorkTask(
                id: id, productID: product.id, projectID: nil, kind: "chore", name: name,
                description: "", status: "active", priority: "medium", ordinal: nil,
                prURL: nil, deletedAt: nil, createdAt: stamp, updatedAt: stamp
            )
            task.shortID = shortID
            return task
        }
        func decision(_ id: String, _ name: String, age: Int, shortID: Int, text: String, askedAt: String) -> WorkTask {
            var task = card(id, name, age: age, shortID: shortID)
            task.status = "blocked"
            task.blockedReason = "awaiting_operator_answer"
            task.operatorQuestion = OperatorQuestion(
                id: "oq_\(id)", text: text, answerType: .yesNo, explanation: "", askedAt: askedAt,
                executionID: "exec_\(id)"
            )
            return task
        }
        func runtime(_ id: String, status: String = "ready", reason: String? = nil,
                     blocker: DispatchWaitBlocker? = nil) -> WorkTaskRuntime {
            WorkTaskRuntime(
                workItemID: id, executionStatus: status, runStatus: nil, executionID: "exec_\(id)",
                dispatchRetryAt: nil, dispatchWaitReason: reason, dispatchWaitSince: nil,
                dispatchWaitBlocker: blocker
            )
        }

        // Task age (the old board order) runs opposite to dispatch order.
        let rows = [
            card("coding", "Implement the new scheduler", age: 1, shortID: 101),
            decision("ask1", "Expand the migration scope", age: 2, shortID: 102,
                     text: "Approve touching 48 files?", askedAt: "1790000100"),
            decision("ask2", "Pick a rollout strategy", age: 3, shortID: 103,
                     text: "Roll out behind a flag?", askedAt: "1790000200"),
            card("retried", "Retried flaky-test fix", age: 4, shortID: 104),
            card("waiting", "Docs follow-up", age: 5, shortID: 105),
            card("paused", "Nightly triage report", age: 6, shortID: 106),
            card("review", "Review the cache PR", age: 7, shortID: 107),
            card("conflict", "Resolve merge conflict", age: 8, shortID: 108),
        ]
        let blocker = DispatchWaitBlocker(workItemID: "coding", productID: product.id, shortID: 101)
        let runtimes = [
            runtime("conflict"),
            runtime("review"),
            runtime("retried", reason: "pool_exhausted"),
            runtime("paused", reason: "automation_paused"),
            runtime("waiting", status: "waiting_dependency", reason: "waiting_dependency", blocker: blocker),
        ]
        model.applyEventForTest(.workTree(
            product: product, projects: [], tasks: [], chores: rows,
            taskRuntimes: runtimes, dependencies: [], ideas: []
        ))
        model.applyEventForTest(.workerLiveStatesList(
            states: [],
            doingSections: ["coding": .coding],
            queuedDispatch: [
                "conflict": QueuedDispatch(pool: .main, position: 0, nextUp: true),
                "review": QueuedDispatch(pool: .review, position: 1),
                "retried": QueuedDispatch(pool: .main, position: 2),
                "paused": QueuedDispatch(pool: .automation, position: nil),
            ]
        ))
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1320, height: 900),
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
                fputs("queued-order capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }
}
