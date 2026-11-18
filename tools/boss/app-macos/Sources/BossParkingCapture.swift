import AppKit
import Foundation
import SwiftUI
import UpdateCore

/// Quiet, isolated fixture of the real `ContentView` for the parking UI:
/// `--capture-reveal-state=parking-default | parking-shown | parking-sheet
/// --capture-to <png>`.
///
/// `parking-default` is the board as it opens (parked rows left off),
/// `parking-shown` is the same data with "Show parked" on (parked rows in
/// their lifecycle columns beside un-parked ones, each with its chip), and
/// `parking-sheet` overlays the park confirmation sheet's real content on the
/// default board. The sheet is overlaid in the SwiftUI tree because
/// `cacheDisplay` cannot capture a separate sheet window; native context
/// menus cannot be captured at all.
@MainActor
enum BossParkingCapture {
    private static var window: NSWindow?

    static func start(path: String, state: String, updateModel: UpdateModel) {
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let product = WorkProduct(
            id: "park-fixture", name: "Parking", slug: "park-fixture", description: "",
            repoRemoteURL: nil, status: "active", createdAt: "2026-10-01T00:00:00Z", updatedAt: "2026-10-01T00:00:00Z"
        )
        model.products = [product]
        model.selectWorkProduct(product.id)
        model.setNavigationMode(.work)

        let now = Date()
        func stamp(hoursAgo: Double) -> String { String(Int(now.timeIntervalSince1970 - hoursAgo * 3600)) }

        let live = WorkProject(
            id: "proj-live", productID: product.id, name: "Search relevance", slug: "search", description: "", goal: "",
            status: "active", priority: "medium", createdAt: "2026-10-01T00:00:00Z", updatedAt: "2026-10-01T00:00:00Z",
            shortID: 41
        )
        let parkedProject = WorkProject(
            id: "proj-parked", productID: product.id, name: "Billing rewrite", slug: "billing", description: "", goal: "",
            status: "active", priority: "medium", createdAt: "2026-10-01T00:00:00Z", updatedAt: "2026-10-01T00:00:00Z",
            shortID: 42,
            parkedAt: stamp(hoursAgo: 52), parkedBy: "human",
            parkedReason: "focusing on search this month", parkedVia: "direct"
        )

        func task(_ id: String, _ name: String, status: String, project: String?, shortID: Int, pr: String? = nil) -> WorkTask {
            var task = WorkTask(
                id: id, productID: product.id, projectID: project, kind: project == nil ? "chore" : "task", name: name,
                description: "", status: status, priority: "medium", ordinal: shortID,
                prURL: pr, deletedAt: nil, createdAt: "2026-10-01T00:00:00Z", updatedAt: "2026-10-01T00:00:00Z"
            )
            task.shortID = shortID
            return task
        }
        func park(_ task: WorkTask, hoursAgo: Double, reason: String?, viaProject: String? = nil) -> WorkTask {
            var task = task
            task.parkedAt = stamp(hoursAgo: hoursAgo)
            task.parkedBy = "human"
            task.parkedReason = reason
            task.parkedVia = viaProject.map { "project:\($0)" } ?? "direct"
            return task
        }

        func ready(_ task: WorkTask) -> WorkTask {
            var task = task
            task.reviewSection = .ready
            task.readyForReview = true
            return task
        }
        let tasks = [
            ready(task("t-cache", "Warm the cache on deploy", status: "in_review", project: "proj-live", shortID: 205,
                       pr: "https://github.com/example/search/pull/121")),
            task("t-rank", "Tune ranking weights", status: "todo", project: "proj-live", shortID: 201),
            task("t-synonyms", "Add synonym expansion", status: "active", project: "proj-live", shortID: 202),
            park(ready(task("t-facets", "Facet counts on the results page", status: "in_review", project: "proj-live", shortID: 203,
                            pr: "https://github.com/example/search/pull/118")),
                 hoursAgo: 5, reason: "unsure the API is still wanted"),
            park(task("t-stream", "Stream partial results", status: "active", project: "proj-live", shortID: 204),
                 hoursAgo: 0.2, reason: nil),
            park(task("t-invoices", "Invoice PDF export", status: "todo", project: "proj-parked", shortID: 301),
                 hoursAgo: 52, reason: "focusing on search this month", viaProject: "proj-parked"),
            park(task("t-tax", "Regional tax rules", status: "blocked", project: "proj-parked", shortID: 302),
                 hoursAgo: 52, reason: "focusing on search this month", viaProject: "proj-parked"),
        ]
        let chores = [
            task("c-docs", "Refresh onboarding docs", status: "todo", project: nil, shortID: 401),
            park(task("c-logs", "Rotate old audit logs", status: "todo", project: nil, shortID: 402),
                 hoursAgo: 26, reason: "safe to run? check retention policy first"),
        ]
        let runtimes = [
            WorkTaskRuntime(
                workItemID: "t-stream", executionStatus: "running", runStatus: "running", executionID: "exec-stream",
                dispatchRetryAt: nil, dispatchWaitReason: nil, dispatchWaitSince: nil
            ),
        ]
        model.applyEventForTest(.workTree(
            product: product, projects: [live, parkedProject], tasks: tasks, chores: chores,
            taskRuntimes: runtimes, dependencies: [], ideas: []
        ))
        if state == "parking-shown" {
            // Direct assignment: a fixture must not write the user's defaults.
            model.showParked = true
        }

        model.isWorkSidebarHidden = false
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1500, height: 820),
            styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false
        )
        captureWindow.title = "Boss [agent capture]"
        let content = ContentView().environmentObject(model).environmentObject(updateModel)
            .overlay {
                if state == "parking-sheet" {
                    ZStack {
                        Color.black.opacity(0.25)
                        ParkWorkItemSheet(
                            request: ParkRequest(id: "t-facets", name: "Facet counts on the results page", isProject: false),
                            onCancel: {}, onPark: { _ in }
                        )
                        .background(Color(nsColor: .windowBackgroundColor), in: RoundedRectangle(cornerRadius: 10))
                        .overlay(RoundedRectangle(cornerRadius: 10).stroke(.gray.opacity(0.3)))
                        .shadow(radius: 12, y: 3)
                    }
                }
            }
        captureWindow.contentView = NSHostingView(rootView: content)
        window = captureWindow
        captureWindow.contentView?.layoutSubtreeIfNeeded()
        DispatchQueue.main.asyncAfter(deadline: .now() + max(BossCaptureArgs.shared.captureAfter, 1.0)) {
            captureWindow.contentView?.layoutSubtreeIfNeeded()
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                NSApp.terminate(nil)
            } catch {
                fputs("parking capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }
}
