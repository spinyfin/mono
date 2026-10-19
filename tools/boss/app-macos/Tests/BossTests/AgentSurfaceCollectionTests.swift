import AppKit
import PaneLayout
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class AgentSurfaceCollectionTests: XCTestCase {
    func testPageSelectionUpdatesRenderedAgentsWithoutNavigation() throws {
        let grid = AgentsGridModel()
        let states = ["a", "b", "c", "d"].map { run in
            WorkerLiveState(
                name: run, slotId: 1, runId: run, model: "opus", shellPid: 1,
                lastEventAt: nil, currentTool: nil, lastToolEndedAt: nil,
                activity: .working, liveStatus: nil, liveStatusAt: nil,
                recoveryStatus: nil, tmuxHosted: true,
                hostId: WorkerLiveState.localHostId
            )
        }
        grid.apply(states: Array(states.prefix(2)), viewers: [:], interactivePoolSize: 16,
                   automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false)
        XCTAssertEqual(grid.layout.pageCount, 2)
        let recorder = MountRecorder()
        let host = NSHostingView(rootView: PageHarness(grid: grid, recorder: recorder))
        host.frame = CGRect(x: 0, y: 0, width: 500, height: 300)

        func centerColor() throws -> NSColor {
            host.layoutSubtreeIfNeeded()
            let rep = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
            host.cacheDisplay(in: host.bounds, to: rep)
            return try XCTUnwrap(rep.colorAt(x: rep.pixelsWide / 2, y: rep.pixelsHigh / 2)?
                .usingColorSpace(.deviceRGB))
        }

        let first = try centerColor()
        XCTAssertEqual(pageValues(in: host)["Page 1, 1 agents"], "Selected")
        XCTAssertGreaterThan(first.redComponent, 0.9)
        XCTAssertLessThan(first.blueComponent, 0.1)
        let originals = recorder.views
        XCTAssertEqual(originals.count, 2)
        // Same action as the page button. Do not replace the host/root view,
        // toggle visibility, refresh membership, or navigate elsewhere.
        grid.selectPage(1)
        let second = try centerColor()
        XCTAssertEqual(pageValues(in: host)["Page 2, 1 agents"], "Selected")
        XCTAssertEqual(pageValues(in: host)["Page 1, 1 agents"], "Not selected")
        XCTAssertGreaterThan(second.blueComponent, 0.9)
        XCTAssertLessThan(second.redComponent, 0.1)
        grid.selectPage(0)
        XCTAssertGreaterThan(try centerColor().redComponent, 0.9)
        XCTAssertEqual(recorder.created, 2)
        XCTAssertEqual(recorder.dismantled, 0)
        for (run, native) in originals { XCTAssertTrue(recorder.views[run] === native) }

        grid.apply(states: [states[1]], viewers: [:], interactivePoolSize: 16,
                   automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false)
        _ = try centerColor()
        XCTAssertTrue(pageValues(in: host).isEmpty, "Controls disappear when only one page remains")
        grid.apply(states: states, viewers: [:], interactivePoolSize: 16,
                   automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false)
        _ = try centerColor()
        XCTAssertEqual(pageValues(in: host)["Page 2, 1 agents"], "Not selected")

        grid.noteArea(width: 1000, height: 600, inLiveResize: true)
        grid.noteCellSize(width: 6, height: 13)
        grid.liveResizeDidEnd()
        _ = try centerColor()
        XCTAssertEqual(pageValues(in: host)["Page 1, 2 agents"], "Selected")
        grid.apply(states: Array(states.dropFirst()), viewers: [:], interactivePoolSize: 16,
                   automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false)
        _ = try centerColor()
        XCTAssertEqual(pageValues(in: host)["Page 1, 1 agents"], "Selected")
        grid.apply(states: states, viewers: [:], interactivePoolSize: 16,
                   automationPoolSize: 8, reviewPoolSize: 16, feedUnavailable: false)
        _ = try centerColor()
        XCTAssertEqual(pageValues(in: host)["Page 1, 2 agents"], "Selected")
    }

    private func pageValues(in view: NSView) -> [String: String] {
        var values: [String: String] = [:]
        if let probe = view as? PageProbeView {
            values[probe.label] = probe.selected ? "Selected" : "Not selected"
        }
        for child in view.subviews {
            values.merge(pageValues(in: child)) { _, new in new }
        }
        return values
    }

    func testLayoutChangesNeverRecreateAttachedNativeViews() {
        let recorder = MountRecorder()
        var ids = (0..<9).map { "run-\($0)" }
        func view(columns: Int, visible: Bool) -> some View {
            let currentIds = ids
            var layout = PaneLayoutModel(geometry: PaneGeometry(
                area: PaneSize(width: Double(columns) * 500, height: 1200),
                cell: PaneSize(width: 6, height: 13)
            ), isVisible: visible)
            layout.updateMembers(currentIds.map { PaneMember(runId: $0, startedAt: .distantPast) })
            layout.selectPage(visible ? 1 : 0)
            return AgentSurfaceCollection(
                runIds: currentIds, layout: layout, area: CGSize(width: 1200, height: 1200)
            ) { run in
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

private struct PageHarness: View {
    @ObservedObject var grid: AgentsGridModel
    let recorder: MountRecorder

    var body: some View {
        let layout = grid.layout
        VStack {
            AgentPageSelectors(layout: layout, onSelect: grid.selectPage) { selector, selected, action in
                AgentPageSelectorChip(selector: selector, isSelected: selected, onSelect: action)
                    .background(PageProbe(selector: selector, selected: selected))
            }
            GeometryReader { geo in
                AgentSurfaceCollection(runIds: ["a", "b"], layout: layout, area: geo.size) { run in
                    (run == "a" ? Color(red: 1, green: 0, blue: 0) : Color(red: 0, green: 0, blue: 1))
                        .background(MountProbe(runId: run, recorder: recorder))
                }
            }
        }
    }
}


// Read the inputs SwiftUI actually delivers to each rendered chip, without
// depending on accessibility services (unavailable in the hermetic test host).
private struct PageProbe: NSViewRepresentable {
    let selector: PageSelector
    let selected: Bool

    func makeNSView(context: Context) -> PageProbeView { PageProbeView() }

    func updateNSView(_ view: PageProbeView, context: Context) {
        view.label = "Page \(selector.index + 1), \(selector.occupiedCount) agents"
        view.selected = selected
    }
}

private final class PageProbeView: NSView {
    var label = ""
    var selected = false
}
