import AppKit
import Foundation
import SwiftUI
import UpdateCore

/// Quiet, isolated fixture: --capture-reveal-state=queued-order --capture-to.
/// Hosts the real ContentView on a Doing column holding a coding card, two
/// operator decisions, and a queue in engine dispatch order (the oldest task
/// is deliberately not first), plus cards the dispatcher is not taking: each
/// with the wait reason the engine sends, and one the engine could not name.
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
        // `label` is the phrase the engine sends with each reason.
        func runtime(_ id: String, status: String = "ready", reason: String = "awaiting_dispatch",
                     label: String = "ready for the dispatcher's next pass",
                     blocker: DispatchWaitBlocker? = nil,
                     notBefore: String? = nil,
                     failures: [DispatchFailureAttempt] = []) -> WorkTaskRuntime {
            var value = WorkTaskRuntime(
                workItemID: id, executionStatus: status, runStatus: nil, executionID: "exec_\(id)",
                dispatchRetryAt: nil, dispatchWaitReason: reason, dispatchWaitSince: nil,
                dispatchWaitBlocker: blocker, dispatchWaitLabel: label
            )
            value.dispatchNotBefore = notBefore
            value.dispatchFailures = failures
            return value
        }

        // Task creation order runs opposite to dispatch order, so the capture shows
        // Queued following the engine's positions rather than task age.
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
            card("ordered", "Second task of its project", age: 9, shortID: 109),
            card("unnamed", "Waiting with no reason the engine can name", age: 10, shortID: 110),
            card("backoff", "Retry after hung worker", age: 11, shortID: 111),
        ]
        let blocker = DispatchWaitBlocker(workItemID: "coding", productID: product.id, shortID: 101)
        let runtimes = [
            runtime("conflict"),
            runtime("review"),
            runtime("retried", reason: "pool_exhausted", label: "worker pool full"),
            runtime("paused", reason: "automation_paused", label: "automation paused"),
            runtime("waiting", status: "waiting_dependency", reason: "waiting_dependency",
                    label: "waiting for \(blocker.label)", blocker: blocker),
            runtime("ordered", status: "waiting_dependency", reason: "project_order",
                    label: "waiting for \(blocker.label), an earlier task in its project", blocker: blocker),
            // What a card shows when the engine could not classify its wait.
            runtime("unnamed", status: "waiting_dependency", reason: "unclassified",
                    label: "the engine has no reason recorded for this wait"),
            runtime(
                "backoff",
                reason: "failure_backoff",
                label: "Retrying after error (attempt 4)",
                notBefore: "1900000000",
                failures: [
                    DispatchFailureAttempt(
                        errorText: "Driver has made no observable progress while awaiting model output",
                        happenedAt: "1899996400"
                    ),
                    DispatchFailureAttempt(
                        errorText: "tmux session is absent from list-sessions",
                        happenedAt: "1899998200"
                    ),
                ]
            ),
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
            // Tall enough for every queued card, down to the unnamed wait.
            contentRect: NSRect(x: 0, y: 0, width: 1320, height: 1480),
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
