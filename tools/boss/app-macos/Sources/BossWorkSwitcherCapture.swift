import AppKit
import SwiftUI
import UpdateCore

/// Quiet fixtures of the real navigation and switcher content. The open
/// popover content is overlaid in the SwiftUI tree (never added to AppKit's
/// window frame, which perturbs the toolbar) because cacheDisplay cannot
/// capture a separate popup window without presenting it on the desktop.
/// `--capture-switcher-closed` omits the menu; `--capture-narrow` uses a
/// 1000pt-wide window.
@MainActor
enum BossWorkSwitcherCapture {
    private static var window: NSWindow?

    static func start(path: String, hidden: Bool, updateModel: UpdateModel) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let names = ["Boss", "Checkleft", "Cube", "Repobin", "Website"]
        let products = names.map {
            WorkProduct(id: $0, name: $0, slug: $0.lowercased(), description: "", repoRemoteURL: nil,
                        status: "active", createdAt: "", updatedAt: "")
        }
        model.products = products
        for name in names.reversed() { model.selectWorkProduct(name) }
        let projects = ["Toolbar navigation", "Board performance", "Worker reliability", "Release tooling", "Review workflow"].map {
            WorkProject(id: $0, productID: "Boss", name: $0, slug: $0, description: "", goal: "",
                        status: "active", priority: "medium", createdAt: "", updatedAt: "")
        }
        let tasks = ["Preserve keyboard navigation", "Keep selection in sync", "Restore last selection on launch",
                     "Show product name in the title", "Add recents to the command palette", "Cover hidden-sidebar layouts",
                     "Document the shortcut", "Persist recents across restarts"].enumerated().map { index, name in
            WorkTask(id: "switcher-card-\(index)", productID: "Boss", projectID: projects[0].id, kind: "task",
                     name: name, description: "", status: index == 0 ? "todo" : "active", priority: "medium",
                     ordinal: index, prURL: nil, deletedAt: nil, createdAt: "", updatedAt: "")
        }
        model.applyEventForTest(.workTree(product: products[0], projects: projects, tasks: tasks, chores: [],
                                        taskRuntimes: [], dependencies: [], ideas: []))
        for project in projects.reversed() {
            model.toggleProjectFilter(project.id)
            model.clearProjectFilters()
        }
        model.toggleProjectFilter(projects[0].id)
        model.isWorkSidebarHidden = hidden
        model.setNavigationMode(.work, refreshWorkOnEntry: false)
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0,
                                width: CommandLine.arguments.contains("--capture-narrow") ? 1000 : 1500, height: 800),
            styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
        captureWindow.title = "Boss [agent capture]"
        let content = ContentView().environmentObject(model).environmentObject(updateModel)
            .overlay(alignment: .topLeading) {
                if hidden && !CommandLine.arguments.contains("--capture-switcher-closed") {
                    WorkSwitcherMenu(model: model, dismiss: {})
                        .background(Color(nsColor: .windowBackgroundColor), in: RoundedRectangle(cornerRadius: 10))
                        .overlay(RoundedRectangle(cornerRadius: 10).stroke(.gray.opacity(0.3)))
                        .shadow(radius: 8, y: 2)
                        .padding(.leading, 90)
                        .padding(.top, 8)
                }
            }
        captureWindow.contentView = NSHostingView(rootView: content)
        window = captureWindow
        DispatchQueue.main.asyncAfter(deadline: .now() + BossCaptureArgs.shared.captureAfter) {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) {
                do {
                    try BossWindowCapture.captureWindow(captureWindow, to: path)
                    NSApp.terminate(nil)
                } catch {
                    fputs("switcher capture failed: \(error)\n", stderr)
                    exit(1)
                }
            }
        }
    }
}
