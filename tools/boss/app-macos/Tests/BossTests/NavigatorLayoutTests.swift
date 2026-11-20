import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class NavigatorLayoutTests: XCTestCase {
    private func context(_ model: ChatViewModel) -> NavigatorDetailContext {
        NavigatorDetailContext(model: model, root: model.navigatorSelectedTask!, ask: nil, isReadOnly: false)
    }

    private func model(_ tasks: [WorkTask]) -> ChatViewModel {
        let model = NavigatorTestFixture.model(tasks)
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        return model
    }

    func testDocumentPickerChoices() {
        XCTAssertFalse(WorkNavigatorBrief.hasMultipleDocuments(rootID: "root", askID: nil, document: .original))
        XCTAssertFalse(WorkNavigatorBrief.hasMultipleDocuments(rootID: "root", askID: "root", document: .original))
        XCTAssertTrue(WorkNavigatorBrief.hasMultipleDocuments(rootID: "root", askID: nil, document: .historical))
        let root = NavigatorTestFixture.task()
        let single = model([root])
        XCTAssertFalse(WorkNavigatorBrief(context: context(single)).hasMultipleDocuments)
        let ask = NavigatorTestFixture.task("revision", rootID: "root")
        let multiple = model([root, ask])
        XCTAssertTrue(WorkNavigatorBrief(context: context(multiple)).hasMultipleDocuments)
        single.navigator.historicalDocumentByRoot[root.id] = ask.id
        NavigatorTestFixture.snapshot(single, [root, ask])
        single.setNavigatorDocument(.historical, rootID: root.id)
        XCTAssertTrue(WorkNavigatorBrief(context: context(single)).hasMultipleDocuments)
    }

    func testActivityRowOnlyExistsForChoicesOrEndedNotice() {
        let root = NavigatorTestFixture.task()
        let model = model([root])
        XCTAssertFalse(WorkNavigatorActivity(context: context(model), workspace: nil).showsActivityPickerRow)
        model.setNavigatorActivity(executionID: "execution", rootID: root.id)
        var resting = root
        resting.navigator = NavigatorProjection.parse([
            "version": 1, "root_task_id": "root", "root": [
                "section": "backlog", "sort_key": "0", "default_detail": "activity",
                "reason": ["type": "active"], "reason_label": "Waiting",
                "revisions": ["open": 0, "total": 0], "activities": [],
            ],
        ])
        NavigatorTestFixture.snapshot(model, [resting])
        let ended = WorkNavigatorActivity(context: context(model), workspace: nil)
        XCTAssertNotNil(ended.endedNotice)
        XCTAssertTrue(ended.showsActivityPickerRow)
    }

    func testLocalPaneRequiresHostedViewerAndExpandsAfterAttach() throws {
        let model = model([NavigatorTestFixture.task()])
        let context = context(model)
        let workspace = WorkersWorkspaceModel()
        let view = WorkNavigatorActivity(context: context, workspace: workspace)
        XCTAssertFalse(view.hasAgentPane)
        XCTAssertFalse(WorkNavigatorActivity(context: context, workspace: nil).hasAgentPane)
        let selected = try XCTUnwrap(model.navigatorSelectedActivity(for: context.root))
        let layout = NavigatorLocalAgentLayout(context: context, selected: selected, ended: false,
                                              isLive: true, isActive: true, workspace: workspace)
        XCTAssertFalse(layout.hasAgentPane)
        var requested: [String] = []
        workspace.onViewerAttachRequested = { requested.append($0) }
        let slot = WorkNavigatorTerminalSlot(selected: selected, ended: false, isLive: true,
                                            isActive: true, workspace: workspace)
        slot.updateAttachRequest(owner: "layout-test")
        XCTAssertEqual(requested, ["run"])
        _ = workspace.attachWorkerPane(EngineAttachRequest(
            runId: "run", slotId: 1, sessionName: "boss-1-run",
            tmuxSocketPath: "/state/boss/tmux.sock", summary: nil, taskTitle: nil
        ))
        let session = try XCTUnwrap(workspace.viewer(forRunId: "run")?.session)
        XCTAssertFalse(layout.hasAgentPane, "A viewer without a host still uses a slim row")
        let host = GhosttyTerminalHostView(
            runtime: GhosttyRuntime.shared, session: session, launchSpec: session.launchSpec,
            paneMonitorEnabled: false, surfaceCreationEnabled: false
        )
        session.hostView = host
        session.terminalReady = true
        XCTAssertTrue(layout.hasAgentPane)
        XCTAssertTrue(view.hasAgentPane)
        session.hostView = nil
        XCTAssertFalse(layout.hasAgentPane)
        XCTAssertEqual(slot.presentation, .unavailable)
        _ = workspace.attachWorkerPane(EngineAttachRequest(
            runId: "run", slotId: 1, sessionName: "boss-1-run",
            tmuxSocketPath: "invalid", summary: nil, taskTitle: nil
        ))
        XCTAssertEqual(slot.presentation, .attachFailure("the engine supplied an invalid tmux socket path"))
    }

    func testNoSelectionHasNoPane() {
        var root = NavigatorTestFixture.task()
        root.navigator = nil
        let model = model([root])
        let context = NavigatorDetailContext(model: model, root: root, ask: nil, isReadOnly: false)
        XCTAssertFalse(WorkNavigatorActivity(context: context, workspace: WorkersWorkspaceModel()).hasAgentPane)
    }

    func testCollapsedWorkspaceLayoutsRenderAtNarrowAndWideWidths() throws {
        for multiple in [false, true] {
            let root = NavigatorTestFixture.task()
            let tasks = multiple ? [root, NavigatorTestFixture.task("revision", rootID: "root")] : [root]
            let model = model(tasks)
            model.isConnected = true
            let workspace = WorkersWorkspaceModel()
            for width in [400, 900] {
                for dark in [false, true] {
                    let host = NSHostingView(rootView: WorkNavigatorActivity(context: context(model), workspace: workspace)
                        .padding(12).frame(width: CGFloat(width), height: 600)
                        .background(Color(nsColor: .windowBackgroundColor))
                        .environment(\.colorScheme, dark ? .dark : .light))
                    host.appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
                    host.frame = NSRect(x: 0, y: 0, width: width, height: 600)
                    for _ in 0..<3 {
                        host.layoutSubtreeIfNeeded()
                        RunLoop.current.run(until: Date().addingTimeInterval(0.02))
                    }
                    let rep = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
                    host.cacheDisplay(in: host.bounds, to: rep)
                    let data = try XCTUnwrap(rep.representation(using: .png, properties: [:]))
                    XCTAssertGreaterThan(data.count, 100)
                    if let output = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"] {
                        let name = "navigator-collapsed-\(multiple ? "multiple" : "single")-\(width)-\(dark ? "dark" : "light").png"
                        try data.write(to: URL(fileURLWithPath: output).appendingPathComponent(name))
                    }
                }
            }
        }
    }
}
