import AppKit
import PaneLayout
import SwiftUI
import XCTest
@testable import Boss

/// Drives the real `WorkerSlotView` (with the production grid's
/// `renderRunIds` / `placement`) through occupied -> placeholder -> unplaced
/// -> detach, asserting the terminal representable is created once and only
/// dismantled on detach.
@MainActor
final class WorkerSlotViewLifecycleTests: XCTestCase {
    func testTerminalRepresentableCreatedOnceAndDismantledOnlyOnDetach() throws {
        let workspace = WorkersWorkspaceModel()
        _ = workspace.attachWorkerPane(EngineAttachRequest(
            runId: "run-a", slotId: 1, sessionName: "boss-1-run-a",
            tmuxSocketPath: "/state/boss/tmux.sock", summary: nil, taskTitle: nil
        ))
        let session = try XCTUnwrap(workspace.viewersByRunId["run-a"]?.session)
        let grid = AgentsGridModel(
            policy: PaneCapacityPolicy(minColumns: 1, minRows: 1, headerHeight: 0, maxPanesPerPage: 8, gap: 0, deadBand: 0),
            geometry: PaneGeometry(area: PaneSize(width: 400, height: 200), cell: PaneSize(width: 100, height: 100))
        )
        let recorder = TerminalRecorder()
        let area = CGSize(width: 400, height: 200)

        func apply(placed: Bool) {
            grid.apply(
                states: placed ? [makeState(runId: "run-a")] : [],
                viewers: workspace.viewersByRunId,
                interactivePoolSize: 16, automationPoolSize: 8, reviewPoolSize: 16
            )
        }
        func rootView(placeholder: Bool) -> some View {
            AgentSurfaceCollection(
                runIds: grid.renderRunIds(attached: workspace.viewersByRunId.keys),
                placement: { grid.placement(for: $0, area: area) }
            ) { runId in
                WorkerSlotView(
                    runtime: workspace.runtime,
                    snapshot: WorkerSlotSnapshot(
                        slotId: 1, runId: runId, summary: nil, taskTitle: nil,
                        session: workspace.viewersByRunId[runId]?.session,
                        liveStatusEnabled: true, live: nil,
                        isPlaceholder: placeholder, cachedPersona: nil
                    ),
                    onToggleLiveStatus: { _ in },
                    terminalContent: { _, session, _ in
                        AnyView(TerminalProbe(sessionId: session.id, recorder: recorder))
                    }
                )
                .equatable()
            }
        }

        // Occupied.
        apply(placed: true)
        let host = NSHostingView(rootView: rootView(placeholder: false))
        host.frame = CGRect(origin: .zero, size: area)
        host.layoutSubtreeIfNeeded()
        XCTAssertEqual(recorder.created, 1)
        XCTAssertEqual(recorder.dismantled, 0)
        XCTAssertTrue(grid.placement(for: "run-a", area: area).visible)

        // Placeholder (engine-released cell still holding the surface).
        host.rootView = rootView(placeholder: true)
        host.layoutSubtreeIfNeeded()
        XCTAssertEqual(recorder.created, 1)
        XCTAssertEqual(recorder.dismantled, 0)

        // Unplaced: membership gone, surface retained hidden.
        apply(placed: false)
        XCTAssertFalse(grid.placement(for: "run-a", area: area).visible)
        host.rootView = rootView(placeholder: true)
        host.layoutSubtreeIfNeeded()
        XCTAssertEqual(recorder.created, 1)
        XCTAssertEqual(recorder.dismantled, 0)

        // Detach is the only teardown.
        _ = workspace.detachWorkerPane(runId: "run-a")
        apply(placed: false)
        host.rootView = rootView(placeholder: true)
        host.layoutSubtreeIfNeeded()
        XCTAssertEqual(recorder.created, 1)
        XCTAssertEqual(recorder.dismantled, 1)
        XCTAssertEqual(recorder.sessionIds, [session.id])
    }

    private func makeState(runId: String) -> WorkerLiveState {
        WorkerLiveState(
            name: nil, slotId: 1, runId: runId, model: "opus", shellPid: 1,
            lastEventAt: nil, currentTool: nil, lastToolEndedAt: nil,
            activity: .working, liveStatus: nil, liveStatusAt: nil,
            recoveryStatus: nil, tmuxHosted: true, agentType: "coding",
            hostId: WorkerLiveState.localHostId, startedAt: "2026-10-09T00:00:00Z",
            pool: "main"
        )
    }
}

@MainActor
private final class TerminalRecorder {
    var created = 0
    var dismantled = 0
    var sessionIds: [String] = []
}

private struct TerminalProbe: NSViewRepresentable {
    let sessionId: String
    let recorder: TerminalRecorder
    func makeCoordinator() -> TerminalRecorder { recorder }
    func makeNSView(context: Context) -> NSView {
        recorder.created += 1
        recorder.sessionIds.append(sessionId)
        return NSView()
    }
    func updateNSView(_ view: NSView, context: Context) {}
    static func dismantleNSView(_ view: NSView, coordinator: TerminalRecorder) {
        coordinator.dismantled += 1
    }
}
