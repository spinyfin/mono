import AppKit
import SwiftUI

/// Quiet, isolated fixture: --capture-reveal-state=agent-pane-activity --capture-to.
///
/// Hosts the real Agents-pane slot header twice, fed exactly the live state
/// the engine now publishes for a Codex worker whose turn is in progress
/// while it waits on its own background command (`activity = working`, no
/// tool in flight), and for the same worker once its driver reported the
/// turn boundary (`activity = idle`). The pill is the one the operator reads
/// on the board; this fixture shows what each engine state renders as, it
/// does not exercise the engine's re-adoption path (that is covered by the
/// engine's own tests).
@MainActor
enum BossAgentPaneActivityCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let midTurn = liveState(
            activity: .working,
            liveStatus: "Running checkleft before pushing the PR",
            lastEventAt: "2026-10-23T19:27:22Z"
        )
        let turnEnded = liveState(
            activity: .idle,
            liveStatus: "Opened the PR and ended its turn",
            lastEventAt: "2026-10-23T19:41:05Z"
        )
        let view = HStack(alignment: .top, spacing: 32) {
            column(
                title: "Mid-turn, waiting on its own background command",
                caption: "Turn open (no Stop from the driver); last tool call balanced. Pill must read Working.",
                live: midTurn
            )
            column(
                title: "Turn ended (driver reported Stop)",
                caption: "The one signal that parks a worker at its prompt. Pill reads Idle.",
                live: turnEnded
            )
        }
        .padding(24)
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1120, height: 360),
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
                fputs("agent pane activity capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }

    private static func column(title: String, caption: String, live: WorkerLiveState) -> some View {
        let snapshot = WorkerSlotSnapshot.render(
            runId: live.runId,
            viewer: WorkerSlot(
                slotId: live.slotId,
                runId: live.runId,
                summary: "resolving a merge conflict on mono PR #3242",
                taskTitle: nil
            ),
            liveState: live,
            liveStatusDisabledSlotIDs: [],
            cachedPersona: nil
        )
        return VStack(alignment: .leading, spacing: 10) {
            Text(title).font(.headline)
            WorkerSlotView(
                runtime: GhosttyRuntime.shared,
                snapshot: snapshot,
                onToggleLiveStatus: { _ in }
            )
            .frame(height: 220)
            .clipShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 6, style: .continuous).strokeBorder(Color.secondary.opacity(0.3)))
            Text(caption).font(.caption).foregroundStyle(.secondary)
        }
        .frame(width: 500)
    }

    private static func liveState(activity: WorkerActivity, liveStatus: String, lastEventAt: String) -> WorkerLiveState {
        WorkerLiveState(
            name: "Riker",
            slotId: 3,
            runId: "capture-codex-run",
            model: "gpt-5-codex",
            shellPid: 0,
            lastEventAt: lastEventAt,
            currentTool: nil,
            lastToolEndedAt: nil,
            activity: activity,
            liveStatus: liveStatus,
            liveStatusAt: lastEventAt,
            recoveryStatus: nil,
            tmuxHosted: true,
            waitReason: nil,
            waitExpiresAt: nil,
            agentType: "coding",
            projectId: nil,
            projectName: "Boss",
            hostId: nil,
            startedAt: "2026-10-23T18:50:00Z",
            pool: nil,
            viewerAttachError: nil
        )
    }
}
