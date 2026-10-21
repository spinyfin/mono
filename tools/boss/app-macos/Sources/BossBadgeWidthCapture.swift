import AppKit
import SwiftUI

/// Quiet real-card fixture: --capture-reveal-state=badge-width --capture-to.
@MainActor
enum BossBadgeWidthCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        var task = WorkTask(
            id: "badge-fixture", productID: "fixture", projectID: "project", kind: "task",
            name: "Long badge labels", description: "", status: "todo", priority: "high",
            ordinal: 0, prURL: nil, deletedAt: nil,
            createdAt: "2026-10-19T12:00:00Z", updatedAt: "2026-10-19T12:00:00Z"
        )
        task.tags = ["WWWWWWWWWWWWWWWWWWWWWWWW", "Short"]
        let snapshot = WorkCardSnapshot.build(task: task, context: WorkCardSnapshotContext(
            column: .doing,
            projectName: "Boss-ism leakage: generic regex check in checkleft with a very long project label",
            liveState: WorkerLiveState(name: "Spock", slotId: 1, runId: "capture-agent", model: "fixture", shellPid: 0, lastEventAt: nil, currentTool: nil, lastToolEndedAt: nil, activity: .working, liveStatus: nil, liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true),
            liveStatus: "Running bazel tests for engine core",
            repoChip: RepoChipPresentation(
                shortName: "a-very-long-repository-name-that-exceeds-the-card-content-width",
                fullURL: "https://github.com/example/repository", provenance: .taskOverride
            )
        ))
        let view = HStack(alignment: .top, spacing: 32) {
            ForEach([CGFloat(280), 340], id: \.self) { width in
                VStack(alignment: .leading, spacing: 12) {
                    Text("Lane: \(Int(width)) pt").font(.headline)
                    WorkBoardCardView(snapshot: snapshot, onFocusAgent: {})
                    Spacer(minLength: 0)
                }
                .padding(14)
                .frame(width: width)
                .background(Color(nsColor: .windowBackgroundColor))
            }
        }.padding(24)
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1000, height: 420),
            styleMask: [.titled], backing: .buffered, defer: false
        )
        captureWindow.title = "Boss [agent capture]"
        captureWindow.contentView = NSHostingView(rootView: view)
        window = captureWindow
        DispatchQueue.main.asyncAfter(deadline: .now() + BossCaptureArgs.shared.captureAfter) {
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                NSApp.terminate(nil)
            } catch {
                fputs("badge capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }
}
