import AppKit
import Foundation
import SwiftUI
import UpdateCore

/// Quiet fixtures of the work-board search chrome: `--capture-reveal-state=
/// work-search-idle | work-search-filter | work-search-reveal |
/// work-search-agents | work-search-find --capture-to`.
///
/// `work-search-idle` is the board with SwiftUI's toolbar search empty.
/// `work-search-filter` types a short id so the filter banner and matching
/// parent+revision cards are visible (the revision has no matching text).
/// `work-search-reveal` starts from that
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
            "task-revision", "Improve search ranking", shortID: 2900, status: "active",
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
        case "work-search-idle", "work-search-find":
            model.workSearchText = ""
        case "work-search-filter":
            model.workSearchText = "2801"
            precondition(model.visibleWorkItems.contains { $0.id == revision.id })
            precondition(!model.visibleWorkItems.contains { $0.id == unrelated.id })
        case "work-search-reveal":
            model.workSearchText = "2801"
            precondition(!model.visibleWorkItems.contains { $0.id == unrelated.id })
            fputs("Reveal target absent before reveal under query 2801\n", stderr)
            model.revealWorkCard(unrelated.id, productID: product.id)
            precondition(model.workSearchText.isEmpty)
            precondition(model.visibleWorkItems.contains { $0.id == unrelated.id })
        case "work-search-agents":
            model.workSearchText = "2801"
            model.setNavigationMode(.agents, refreshWorkOnEntry: false)
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
            if state == "work-search-find" {
                let event = NSEvent.keyEvent(
                    with: .keyDown, location: .zero, modifierFlags: .command,
                    timestamp: 0, windowNumber: captureWindow.windowNumber,
                    context: nil, characters: "f", charactersIgnoringModifiers: "f",
                    isARepeat: false, keyCode: 3
                )!
                precondition(captureWindow.performKeyEquivalent(with: event), "Find shortcut was not handled")
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                finishCapture(captureWindow, path: path, state: state)
            }
        }
    }

    private static func finishCapture(_ captureWindow: NSWindow, path: String, state: String) {
        if state == "work-search-find" {
            guard let editor = captureWindow.firstResponder as? NSTextView,
                  let field = editor.delegate as? NSSearchField,
                  field.placeholderString == "Search tasks…" else {
                fputs("Find did not focus the toolbar search field\n", stderr)
                exit(1)
            }
            fputs("Command-F focused the toolbar search field\n", stderr)
        }
        do {
            try BossWindowCapture.captureWindow(captureWindow, to: path)
            NSApp.terminate(nil)
        } catch {
            fputs("work-search capture failed: \(error)\n", stderr)
            exit(1)
        }
    }
}
