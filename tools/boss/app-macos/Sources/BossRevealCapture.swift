import AppKit
import Foundation
import SwiftUI
import UpdateCore

/// Repeatable real-view reveal validation in the existing quiet capture app.
/// Pass --capture-reveal-state=done (or another lifecycle state) alongside
/// --capture-to. Fixtures use the isolated socket and defaults only.
@MainActor
enum BossRevealCapture {
    private static var window: NSWindow?
    static var state: String? {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return nil }
        return CommandLine.arguments.first { $0.hasPrefix("--capture-reveal-state=") }
            .map { String($0.dropFirst("--capture-reveal-state=".count)) }
    }

    static func start(updateModel: UpdateModel) {
        guard let state, let path = BossCaptureArgs.shared.captureTo else { return }
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let now = ISO8601DateFormatter().string(from: Date())
        let product = WorkProduct(
            id: "reveal-fixture", name: "Reveal verification", slug: "reveal-fixture",
            description: "", repoRemoteURL: nil, status: "active", createdAt: now, updatedAt: now
        )
        model.products = [product]
        model.selectWorkProduct(product.id)
        model.setNavigationMode(.work)
        var rows = (0..<30).map { index in
            WorkTask(
                id: "reveal-row-\(index)", productID: product.id, projectID: nil, kind: "chore",
                name: index == 29 ? "REVEAL TARGET — \(state)" : "Other card \(index)",
                description: "Reveal viewport fixture", status: state == "missing" ? "todo" : state,
                priority: "medium", ordinal: index,
                prURL: index == 29 ? "https://github.com/spinyfin/mono/pull/3065" : nil,
                deletedAt: nil, createdAt: now, updatedAt: now
            )
        }
        if state == "missing" { rows.removeLast() }
        var runtimes: [WorkTaskRuntime] = []
        if state == "queued" {
            rows = Array(rows.suffix(2))
            rows[0].status = "active"
            rows[0].name = "Earlier review revision"
            rows[0].shortID = 41
            rows[1].status = "todo"
            rows[1].autostart = true
            rows[1].name = "Apply the next review findings"
            rows[1].shortID = 42
            runtimes = [WorkTaskRuntime(
                workItemID: rows[1].id, executionStatus: "waiting_dependency", runStatus: nil,
                executionID: "capture-pending", dispatchRetryAt: nil,
                dispatchWaitReason: "waiting_dependency", dispatchWaitSince: nil,
                dispatchWaitBlocker: DispatchWaitBlocker(
                    workItemID: rows[0].id, productID: product.id, shortID: rows[0].shortID
                )
            )]
        }
        model.applyEventForTest(.workTree(
            product: product, projects: [], tasks: [], chores: rows,
            taskRuntimes: runtimes, dependencies: [], ideas: []
        ))
        for column in WorkBoardColumnKey.allCases {
            for section in model.workSections(in: column) where section.isCollapsible {
                BossDefaults.store.set(
                    section.defaultExpanded,
                    forKey: WorkBoardSectionCollapse.storageKey(sectionID: section.id)
                )
            }
        }
        // Command-line WindowGroup startup can produce no window on macOS.
        // Host the real ContentView explicitly for this opt-in capture only.
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1060, height: 680),
            styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false
        )
        captureWindow.title = "Boss [agent capture]"
        captureWindow.contentView = NSHostingView(rootView: ContentView()
            .environmentObject(model).environmentObject(updateModel))
        window = captureWindow
        captureWindow.contentView?.layoutSubtreeIfNeeded()
        // Let the real board mount with its target section collapsed first.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
            model.revealWorkCard("reveal-row-29", productID: product.id) { result in
                switch result {
                case .success:
                    guard model.revealHighlightID == "reveal-row-29" else {
                        fputs("reveal capture highlighted the wrong card\n", stderr)
                        exit(1)
                    }
                    print("revealed reveal-row-29 (\(state)); exact card visible and highlighted")
                    BossWindowCapture.scheduleCapture(to: path, after: 0.3)
                case .failure(.internalFailure(let reason)):
                    fputs("reveal failed: \(reason)\n", stderr)
                    exit(1)
                }
            }
        }
    }
}
