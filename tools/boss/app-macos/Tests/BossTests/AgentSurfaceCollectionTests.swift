import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class AgentSurfaceCollectionTests: XCTestCase {
    func testLayoutChangesNeverRecreateAttachedNativeViews() {
        let recorder = MountRecorder()
        var ids = (0..<9).map { "run-\($0)" }
        func view(columns: Int, visible: Bool) -> some View {
            let currentIds = ids
            return AgentSurfaceCollection(runIds: currentIds, placement: { run in
                let index = currentIds.firstIndex(of: run)!
                return (CGRect(x: (index % columns) * 120, y: (index / columns) * 120,
                               width: 120, height: 120), visible)
            }) { run in
                MountProbe(runId: run, recorder: recorder)
            }
        }
        let host = NSHostingView(rootView: view(columns: 1, visible: false))
        host.frame = CGRect(x: 0, y: 0, width: 1200, height: 1200)
        host.layoutSubtreeIfNeeded()
        let originals = recorder.views
        XCTAssertEqual(originals.count, 9)
        // Attach-before-snapshot, growth, page switch, Tidy, and compaction
        // all alter frames/order/visibility, never the run-keyed host ancestry.
        for columns in [2, 3, 4, 2] {
            ids.reverse()
            host.rootView = view(columns: columns, visible: true)
            host.layoutSubtreeIfNeeded()
            XCTAssertEqual(recorder.created, 9)
            XCTAssertEqual(recorder.dismantled, 0)
            for (run, native) in originals { XCTAssertTrue(recorder.views[run] === native) }
        }
        ids.removeLast() // Engine detach is the only removal.
        host.rootView = view(columns: 4, visible: false)
        host.layoutSubtreeIfNeeded()
        XCTAssertEqual(recorder.dismantled, 1)
    }
}

@MainActor
private final class MountRecorder {
    var views: [String: NSView] = [:]
    var created = 0
    var dismantled = 0
}

private struct MountProbe: NSViewRepresentable {
    let runId: String
    let recorder: MountRecorder
    func makeCoordinator() -> MountRecorder { recorder }
    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        recorder.views[runId] = view
        recorder.created += 1
        return view
    }
    func updateNSView(_ view: NSView, context: Context) {}
    static func dismantleNSView(_ view: NSView, coordinator: MountRecorder) {
        coordinator.dismantled += 1
    }
}
