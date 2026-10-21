import AppKit
import SwiftUI

/// Quiet real-pane fixture: --capture-reveal-state=exited-worker --capture-to.
@MainActor
enum BossExitedWorkerCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let live = WorkerLiveState(
            name: "Dax", slotId: 4, runId: "capture-worker", model: "codex", shellPid: 0,
            lastEventAt: "2026-10-20T19:42:42Z", currentTool: nil, lastToolEndedAt: nil,
            activity: .terminated, liveStatus: "Running bazel test for github tracker",
            liveStatusAt: "2026-10-20T19:42:11Z", recoveryStatus: nil, tmuxHosted: true,
            waitReason: nil, waitExpiresAt: nil, agentType: "codex", projectId: nil,
            projectName: "Boss", hostId: nil, startedAt: nil, pool: "interactive",
            viewerAttachError: nil
        )
        let snapshot = WorkerSlotSnapshot(
            slotId: 4, runId: live.runId, summary: "working", taskTitle: "Fix worker liveness",
            session: nil, liveStatusEnabled: true, live: WorkerSlotLiveSlice(live),
            isPlaceholder: false, cachedPersona: nil, viewerAttachFailure: nil
        )
        let view = WorkerSlotView(runtime: .shared, snapshot: snapshot, onToggleLiveStatus: { _ in })
            .frame(width: 600, height: 400)
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 600, height: 400),
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
                fputs("exited worker capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }
}
