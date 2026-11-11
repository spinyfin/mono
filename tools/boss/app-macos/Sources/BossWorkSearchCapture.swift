import AppKit
import Foundation
import SwiftUI
import UpdateCore

/// Quiet fixtures of the work-board search chrome: `--capture-reveal-state=
/// work-search-idle | work-search-filter | work-search-reveal --capture-to`.
///
/// `work-search-idle` is the board with SwiftUI's toolbar search empty.
/// `work-search-filter` types a short id so the filter banner and matching
/// parent+revision cards are visible. `work-search-reveal` starts from that
/// hiding filter and reveals a non-matching card, which must clear search.
@MainActor
enum BossWorkSearchCapture {
    private static var window: NSWindow?

    static func start(path: String, state: String, updateModel: UpdateModel) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let now = "2026-05-26T00:00:00Z"
        let product = WorkProduct(
            id: "search-fixture", name: "Search verification", slug: "search-fixture",
            description: "", repoRemoteURL: nil, status: "active", createdAt: now, updatedAt: now
        )
        let project = WorkProject(
            id: "proj-search", productID: product.id, name: "Board search", slug: "board-search",
            description: "", goal: "", status: "active", priority: "medium", createdAt: now, updatedAt: now
        )
        func task(
            _ id: String, _ name: String, shortID: Int, status: String = "todo",
            kind: String = "task", parent: String? = nil
        ) -> WorkTask {
            var task = WorkTask(
                id: id, productID: product.id, projectID: project.id, kind: kind, name: name,
                description: "", status: status, priority: "medium", ordinal: shortID,
                prURL: nil, deletedAt: nil, createdAt: now, updatedAt: now
            )
            task.shortID = shortID
            task.parentTaskId = parent
            return task
        }
        let parent = task("task-parent", "Short-id parent", shortID: 2801, status: "active")
        let revision = task(
            "task-revision", "Revision of 2801", shortID: 2900, status: "active",
            kind: "revision", parent: parent.id
        )
        let unrelated = task("task-unrelated", "Unrelated banana", shortID: 3000, status: "todo")
        let extras = [
            task("task-todo-2", "Write ranking notes", shortID: 3001, status: "todo"),
            task("task-todo-3", "Document the Find shortcut", shortID: 3002, status: "todo"),
            task("task-active-2", "Tune synonym expansion", shortID: 3003, status: "active"),
            task("task-review", "Review the search chrome", shortID: 3004, status: "in_review"),
            task("task-done", "Ship the first filter banner", shortID: 3005, status: "done"),
        ]
        model.products = [product]
        model.selectWorkProduct(product.id)
        model.applyEventForTest(.workTree(
            product: product, projects: [project],
            tasks: [parent, revision, unrelated] + extras, chores: [],
            taskRuntimes: [], dependencies: [], ideas: []
        ))
        model.setNavigationMode(.work, refreshWorkOnEntry: false)
        switch state {
        case "work-search-idle":
            model.workSearchText = ""
        case "work-search-filter":
            model.workSearchText = "2801"
        case "work-search-reveal":
            model.workSearchText = "Banana"
            model.revealWorkCard(unrelated.id, productID: product.id)
        default:
            fputs("unknown work-search capture state: \(state)\n", stderr)
            exit(1)
        }
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1500, height: 800),
            styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false
        )
        captureWindow.title = "Boss [agent capture]"
        captureWindow.contentView = NSHostingView(
            rootView: ContentView().environmentObject(model).environmentObject(updateModel)
        )
        window = captureWindow
        captureWindow.contentView?.layoutSubtreeIfNeeded()
        DispatchQueue.main.asyncAfter(deadline: .now() + max(BossCaptureArgs.shared.captureAfter, 1.0)) {
            captureWindow.contentView?.layoutSubtreeIfNeeded()
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                NSApp.terminate(nil)
            } catch {
                fputs("work-search capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }
}
