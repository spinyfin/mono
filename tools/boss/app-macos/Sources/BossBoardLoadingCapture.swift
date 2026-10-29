import AppKit
import SwiftUI
import UpdateCore

/// Quiet real-view fixture for the board's loading state:
/// `--capture-reveal-state=board-loading --capture-to <png>`.
///
/// Hosts the real `ContentView` on a selected product that has not received
/// its first snapshot, with the engine's cold-build progress seeded the way
/// `board_population_progress` pushes would. Tunables, all optional:
/// - `--capture-board-loading-elapsed=<seconds>` how long the wait has run (47).
/// - `--capture-board-loading-stage=<segment>` last finished stage (`db.task_runtimes`).
/// - `--capture-board-loading-items=<n>` cards counted so far (1949).
/// - `--capture-board-loading-disconnected` shows the offline wording.
@MainActor
enum BossBoardLoadingCapture {
    private static var window: NSWindow?

    private static func option(_ name: String) -> String? {
        CommandLine.arguments.first { $0.hasPrefix("--capture-board-loading-\(name)=") }
            .map { String($0.dropFirst("--capture-board-loading-\(name)=".count)) }
    }

    static func start(path: String, updateModel: UpdateModel) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let now = ISO8601DateFormatter().string(from: Date())
        let product = WorkProduct(
            id: "loading-fixture", name: "Boss", slug: "loading-fixture",
            description: "", repoRemoteURL: nil, status: "active", createdAt: now, updatedAt: now
        )
        model.products = [product]
        model.selectWorkProduct(product.id)
        model.setNavigationMode(.work)

        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1060, height: 680),
            styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false
        )
        captureWindow.title = "Boss [agent capture]"
        captureWindow.contentView = NSHostingView(rootView: ContentView()
            .environmentObject(model).environmentObject(updateModel))
        window = captureWindow
        captureWindow.contentView?.layoutSubtreeIfNeeded()

        // Seed the wait just before the grab: the offline client may report
        // a disconnect while the window mounts, which resets the clock.
        DispatchQueue.main.asyncAfter(deadline: .now() + BossCaptureArgs.shared.captureAfter) {
            let elapsed = option("elapsed").flatMap(TimeInterval.init) ?? 47
            model.isConnected = !CommandLine.arguments.contains("--capture-board-loading-disconnected")
            model.updateBoardLoad {
                $0.noteRequested(product.id, at: Date().addingTimeInterval(-elapsed))
                $0.noteProgress(product.id, BoardLoadProgress(
                    stage: option("stage") ?? "db.task_runtimes",
                    engineElapsedMs: Int(elapsed * 1000),
                    items: option("items").flatMap(Int.init) ?? 1949
                ))
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
                captureWindow.contentView?.layoutSubtreeIfNeeded()
                do {
                    try BossWindowCapture.captureWindow(captureWindow, to: path)
                    NSApp.terminate(nil)
                } catch {
                    fputs("board loading capture failed: \(error)\n", stderr)
                    exit(1)
                }
            }
        }
    }
}
