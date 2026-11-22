import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class NavigatorRowTests: XCTestCase {
    func testRegisteredBodyRetainsLocalStateAcrossMovementAndTabChanges() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task()])
        model.selectWorkCard("root")
        var appearances: [UUID] = []
        var readOnlyValues: [Bool] = []
        var registry = NavigatorDetailRegistry()
        registry.register(.activity) { context in
            readOnlyValues.append(context.isReadOnly)
            return AnyView(NavigatorSlotProbe { appearances.append($0) })
        }
        let host = NSHostingView(rootView: WorkNavigatorDetail(model: model, registry: registry))
        host.frame = NSRect(x: 0, y: 0, width: 600, height: 600)
        func settle() {
            host.layoutSubtreeIfNeeded()
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        }
        settle()
        XCTAssertEqual(appearances.count, 1)
        model.navigator.tab = .history
        NavigatorTestFixture.snapshot(model, [NavigatorTestFixture.task(section: "landed", detail: "review")])
        settle()
        model.navigator.tab = .activity
        settle()
        XCTAssertEqual(appearances.count, 1, "Moving into a collapsed section must not recreate the registered body")
        XCTAssertEqual(readOnlyValues.last, true, "Disconnected integrations must receive read-only context")
        model.isConnected = true
        settle()
        XCTAssertEqual(readOnlyValues.last, false)
    }

    func testWireDecodingIncludesFoldedUnknownAndOmittedFields() throws {
        let projection = try XCTUnwrap(NavigatorTestFixture.task().navigator)
        XCTAssertEqual(projection.root?.activities.first?.executionId, "execution")
        XCTAssertEqual(projection.root?.commands.first?.kind, "open_pr")
        XCTAssertNil(NavigatorTestFixture.task("revision", rootID: "root").navigator?.root)
        let unknown = NavigatorProjection.parse([
            "version": 1, "root_task_id": "root", "root": [
                "section": "backlog", "sort_key": "a", "default_detail": "future_detail",
                "reason": ["type": "future_reason"], "reason_label": "Waiting for something new",
                "revisions": ["open": 0, "total": 0],
            ],
        ])
        XCTAssertEqual(unknown?.root?.reason.type, "future_reason")
        XCTAssertEqual(unknown?.root?.actions, [])
        XCTAssertEqual(unknown?.root?.activities, [])
        XCTAssertEqual(unknown?.root?.commands, [])
    }

    func testCompatibilityDoesNotGuessMissingOrUnknownSections() {
        var legacy = NavigatorTestFixture.task()
        legacy.navigator = nil
        let model = NavigatorTestFixture.model([legacy])
        XCTAssertNotNil(model.navigatorCompatibilityMessage)
        model.selectWorkCard("root")
        model.setWorkViewMode(.navigator)
        XCTAssertNil(model.selectedWorkCardID, "Unsupported tasks have no Navigator selection")
        model.setWorkViewMode(.kanban)
        XCTAssertEqual(model.selectedWorkCardID, "root")
        legacy.sourceAutomationId = "automation"
        NavigatorTestFixture.snapshot(model, [legacy, NavigatorTestFixture.task("supported")])
        XCTAssertNil(model.navigatorCompatibilityMessage)
        XCTAssertEqual(model.navigatorVisibleRows.map(\.id), ["supported"])
        NavigatorTestFixture.snapshot(model, [NavigatorTestFixture.task(section: "future_section")])
        XCTAssertNotNil(model.navigatorCompatibilityMessage)
        XCTAssertTrue(model.navigatorSections.flatMap(\.rows).isEmpty)
    }

    func testParserCarriesProjectionAndFutureVersionDisablesNavigator() throws {
        let client = EngineClient(socketPaths: ["/tmp/navigator-parser.sock"])
        let wire: [String: Any] = [
            "id": "root", "product_id": "product", "kind": "task", "name": "Root", "description": "Brief",
            "status": "todo", "created_at": "", "updated_at": "",
            "navigator": ["version": 2, "root_task_id": "root"],
        ]
        let task = try XCTUnwrap(client.parseTask(wire))
        XCTAssertEqual(task.navigator?.version, 2)
        XCTAssertNotNil(NavigatorTestFixture.model([task]).navigatorCompatibilityMessage)
    }

    func testRowsAtNarrowStandardAndWideWidthsKeepAccessibleIndicators() throws {
        var task = NavigatorTestFixture.task()
        task.name = "A long task title that must truncate without pushing questions or health out of view"
        task.prMergeableState = "conflicting"
        task.ciRequiredState = "fail"
        task.reviewRequiredState = "changes_requested"
        let row = NavigatorRow(task: task, projection: try XCTUnwrap(task.navigator?.root), matchingAskID: nil)
        for width: CGFloat in [260, 360, 520] {
            let presentation = NavigatorRowPresentation(row: row, project: "Navigation", width: width)
            XCTAssertEqual(presentation.showsSecondaryIndicators, width >= 340)
            XCTAssertTrue(presentation.accessibilityLabel.contains("Merge conflict"))
            XCTAssertTrue(presentation.accessibilityLabel.contains("CI fail"))
            XCTAssertTrue(presentation.accessibilityLabel.contains("Review changes requested"))
            XCTAssertTrue(presentation.accessibilityLabel.contains("1 questions"))
            XCTAssertTrue(presentation.accessibilityLabel.contains(task.name))
            let view = WorkNavigatorRow(row: row, project: "Navigation", width: width, isSelected: true, isWorkerActive: false)
                .frame(width: width).background(Color(nsColor: .windowBackgroundColor))
            let host = NSHostingView(rootView: view)
            host.frame = NSRect(origin: .zero, size: host.fittingSize)
            XCTAssertEqual(host.bounds.width, width, accuracy: 1)
            XCTAssertEqual(host.bounds.height, 30, accuracy: 1)
            try capture(host, name: "navigator-row-\(Int(width))")
        }
        let large = NSHostingView(rootView: WorkNavigatorRow(row: row, project: "Navigation", width: 260, isSelected: false, isWorkerActive: false)
            .dynamicTypeSize(.accessibility3).frame(width: 260).background(Color(nsColor: .windowBackgroundColor)))
        large.frame = NSRect(origin: .zero, size: large.fittingSize)
        XCTAssertEqual(large.bounds.width, 260, accuracy: 1)
        XCTAssertEqual(large.bounds.height, 52, accuracy: 1)
        try capture(large, name: "navigator-row-large-text")
    }

    func testWorkingRowAccessibilityDistinguishesActiveAndIdle() throws {
        let task = NavigatorTestFixture.task(section: "working")
        let row = NavigatorRow(task: task, projection: try XCTUnwrap(task.navigator?.root), matchingAskID: nil)
        let idle = NavigatorRowPresentation(row: row, project: nil, width: 260)
        let active = NavigatorRowPresentation(row: row, project: nil, width: 260, isWorkerActive: true)
        XCTAssertTrue(idle.accessibilityLabel.contains("Worker idle"))
        XCTAssertTrue(active.accessibilityLabel.contains("Worker active"))
    }

    func testRevealRepublishesNavigatorChromeWhenSelectionIsAlreadyVisible() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task()])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.revealNavigatorSelection()
        model.presentationUpdates.flush()
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        XCTAssertFalse(model.navigatorCollapsed.contains(.working))
        XCTAssertTrue(model.workSearchText.isEmpty)

        let chrome = WorkPresentationValue(updates: model.presentationUpdates) {
            NavigatorChromeValue(model: model)
        }
        var publications = 0
        let subscription = chrome.objectWillChange.sink { publications += 1 }
        for action in 0..<3 {
            let before = chrome.value
            if action == 0 {
                model.revealNavigatorSelection()
            } else {
                model.moveNavigatorSelection(by: action == 1 ? -1 : 1, edge: true)
            }
            model.presentationUpdates.flush()
            XCTAssertEqual(publications, action + 1, "Each reveal must reach the scroll observer")
            XCTAssertNotEqual(chrome.value.revealGeneration, before.revealGeneration)
            XCTAssertEqual(chrome.value.revealGeneration, model.navigator.revealGeneration)
            XCTAssertEqual(chrome.value.selectedRootID, before.selectedRootID)
            XCTAssertEqual(chrome.value.collapsed, before.collapsed)
            XCTAssertEqual(chrome.value.searchText, before.searchText)
            XCTAssertEqual(chrome.value.sections, before.sections)
        }
        model.presentationUpdates.flush()
        XCTAssertEqual(publications, 3, "A completed reveal must not replay on a later flush")
        withExtendedLifetime(subscription) {}
    }

    func testWorkerTicksDoNotInvalidateNavigatorChromeOrUnrelatedRows() throws {
        let task = NavigatorTestFixture.task("root", section: "working")
        let model = NavigatorTestFixture.model([task])
        func worker(_ run: String, _ activity: WorkerActivity, _ status: String) -> WorkerLiveState {
            WorkerLiveState(slotId: 1, runId: run, model: "fixture", shellPid: 0, lastEventAt: nil,
                currentTool: nil, lastToolEndedAt: nil, activity: activity, liveStatus: status,
                liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true)
        }
        var chromeReads = 0
        let chrome = WorkPresentationValue(updates: model.presentationUpdates) {
            chromeReads += 1
            return NavigatorChromeValue(model: model)
        }
        let row = NavigatorRow(task: task, projection: try XCTUnwrap(task.navigator?.root), matchingAskID: nil)
        let rowState = NavigatorRowLiveState(model: model, row: row)
        var chromeChanges = 0
        var rowChanges = 0
        let subscriptions = [
            chrome.objectWillChange.sink { chromeChanges += 1 },
            rowState.objectWillChange.sink { rowChanges += 1 },
        ]
        for tick in 0..<100 {
            model.liveWorkerStates.update(states: (0..<20).map { worker("other-\($0)", .working, "tick \(tick)") })
            model.presentationUpdates.flush()
        }
        XCTAssertEqual(chromeChanges, 0)
        XCTAssertEqual(chromeReads, 1, "Live-only ticks must not rederive navigator chrome")
        XCTAssertEqual(rowChanges, 0, "A row must ignore other workers' ticks")
        model.liveWorkerStates.update(states: [worker("execution", .working, "go")])
        model.presentationUpdates.flush()
        XCTAssertEqual(rowChanges, 1)
        XCTAssertTrue(rowState.isWorkerActive)
        withExtendedLifetime(subscriptions) {}
    }

    func testActiveDetectionMapsLiveWorkerStateToRow() throws {
        let task = NavigatorTestFixture.task(section: "working")
        let row = NavigatorRow(task: task, projection: try XCTUnwrap(task.navigator?.root), matchingAskID: nil)
        func live(_ id: String, _ activity: WorkerActivity) -> WorkerLiveState {
            WorkerLiveState(slotId: 1, runId: id, model: "model", shellPid: 1, lastEventAt: nil, currentTool: nil,
                            lastToolEndedAt: nil, activity: activity, liveStatus: nil, liveStatusAt: nil,
                            recoveryStatus: nil, tmuxHosted: nil)
        }
        typealias P = NavigatorRowPresentation
        XCTAssertTrue(P.isWorkerActive(row: row, liveStates: ["execution": live("execution", .working)]))
        XCTAssertFalse(P.isWorkerActive(row: row, liveStates: ["execution": live("execution", .idle)]))
        XCTAssertFalse(P.isWorkerActive(row: row, liveStates: ["other": live("other", .working)]))
        XCTAssertFalse(P.isWorkerActive(row: row, liveStates: ["run": live("run", .working)]))
        XCTAssertFalse(P.isWorkerActive(row: row, liveStates: [:]))
    }

    func testKindAndHealthIconsCaptureRepresentativeRows() throws {
        let stack = try iconLegend()
            .padding(12)
            .frame(width: 400)
            .background(Color(nsColor: .windowBackgroundColor))
        let host = NSHostingView(rootView: stack)
        host.frame = NSRect(origin: .zero, size: host.fittingSize)
        try capture(host, name: "navigator-row-icons-light")
        host.appearance = NSAppearance(named: .darkAqua)
        try capture(host, name: "navigator-row-icons-dark")
    }

    func testChoresAndTasksShareAKindSymbolAndRevisionsDoNotCollideWithConflicts() throws {
        XCTAssertEqual(try presentation(kind: "chore").symbol, "wrench.and.screwdriver")
        XCTAssertEqual(try presentation(kind: "task").symbol, "wrench.and.screwdriver")
        XCTAssertEqual(try presentation(kind: "project_task").symbol, "wrench.and.screwdriver")
        XCTAssertEqual(try presentation(kind: "revision").symbol, "arrow.triangle.2.circlepath")
        XCTAssertEqual(try presentation(kind: "design").symbol, "pencil.and.outline")
        XCTAssertEqual(try presentation(kind: "followup").symbol, "arrow.turn.down.right")
        XCTAssertEqual(try presentation(kind: "investigation").symbol, "magnifyingglass")

        let conflict = try presentation(kind: "task", mergeable: "conflicting")
        XCTAssertEqual(conflict.health, NavigatorRowHealth(label: "Merge conflict", symbol: "arrow.triangle.branch", tint: .warning))
        XCTAssertTrue(conflict.accessibilityLabel.contains("Merge conflict"))
        XCTAssertEqual(conflict.healthFacts, ["Merge conflict"])

        let deferred = try presentation(kind: "task", mergeable: "conflicting", deferred: true)
        XCTAssertEqual(deferred.health, conflict.health)
        XCTAssertTrue(deferred.accessibilityLabel.contains("Merge conflict"))
        XCTAssertEqual(deferred.healthFacts, ["Merge conflict", "Conflict resolves on merge"])

        let failed = try presentation(kind: "task", ci: "fail")
        XCTAssertEqual(failed.health, NavigatorRowHealth(label: "CI failed", symbol: "xmark.circle", tint: .failure))
        XCTAssertTrue(failed.accessibilityLabel.contains("CI failed"))

        let revisionConflict = try presentation(kind: "revision", mergeable: "conflicting")
        XCTAssertEqual(revisionConflict.symbol, "arrow.triangle.2.circlepath")
        XCTAssertEqual(revisionConflict.health?.symbol, "arrow.triangle.branch")
        XCTAssertNotEqual(revisionConflict.symbol, revisionConflict.health?.symbol)
    }

    func testAccessibilityAnnouncesEachCIStateOnceWithAndWithoutConflict() throws {
        for (state, label) in [("fail", "CI failed"), ("in_progress", "CI pending"), ("success", "CI passed")] {
            for mergeable in ["mergeable", "conflicting"] {
                let row = try presentation(kind: "task", mergeable: mergeable, ci: state)
                let parts = row.accessibilityLabel.components(separatedBy: ", ")
                XCTAssertEqual(parts.filter { $0.hasPrefix("CI ") }, [label])
                XCTAssertTrue(row.healthFacts.contains(label))
                if mergeable == "conflicting" {
                    XCTAssertEqual(parts.filter { $0 == "Merge conflict" }.count, 1)
                }
            }
        }
    }

    func testHealthPriorityKeepsSuppressedFactsAccessible() throws {
        var task = NavigatorTestFixture.task()
        task.prMergeableState = "conflicting"
        task.ciRequiredState = "fail"
        task.reviewRequiredState = "changes_requested"
        func presentation() throws -> NavigatorRowPresentation {
            NavigatorRowPresentation(row: NavigatorRow(task: task, projection: try XCTUnwrap(task.navigator?.root),
                                                       matchingAskID: nil), project: nil, width: 260)
        }
        XCTAssertEqual(try presentation().health, NavigatorRowHealth(label: "Merge conflict", symbol: "arrow.triangle.branch", tint: .warning))
        task.conflictResolutionDeferred = true
        XCTAssertEqual(try presentation().health, NavigatorRowHealth(label: "Merge conflict", symbol: "arrow.triangle.branch", tint: .warning))
        XCTAssertTrue(try presentation().accessibilityLabel.contains("Merge conflict"))
        XCTAssertTrue(try presentation().healthFacts.contains("Conflict resolves on merge"))
        task.conflictResolutionDeferred = false
        task.prMergeableState = "mergeable"
        XCTAssertEqual(try presentation().health, NavigatorRowHealth(label: "CI failed", symbol: "xmark.circle", tint: .failure))
        XCTAssertTrue(try presentation().accessibilityLabel.contains("CI failed"))
        task.ciRequiredState = "in_progress"
        XCTAssertEqual(try presentation().health?.label, "Review changes requested")
        task.reviewRequiredState = "approved"
        XCTAssertEqual(try presentation().health?.label, "CI pending")
        XCTAssertTrue(try presentation().accessibilityLabel.contains("Review approved"))
        task.ciRequiredState = "success"
        XCTAssertEqual(try presentation().health?.label, "Review approved")
        task.reviewRequiredState = nil
        XCTAssertEqual(try presentation().health?.label, "CI passed")
        task.ciRequiredState = "unknown"
        XCTAssertNil(try presentation().health)
        XCTAssertTrue(try presentation().accessibilityLabel.contains("CI unknown"))
    }

    func testDetailSlotsAndNavigatorRenderWithoutProductionEngine() throws {
        var root = NavigatorTestFixture.task()
        root.prMergeableState = "conflicting"
        root.ciRequiredState = "fail"
        root.reviewRequiredState = "changes_requested"
        let model = NavigatorTestFixture.model([root, NavigatorTestFixture.task("revision", rootID: "root")])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        for slot in NavigatorDetailSlot.allCases {
            model.navigator.tab = slot
            let host = NSHostingView(rootView: WorkNavigatorView(model: model).frame(width: 1000, height: 650))
            host.frame = NSRect(x: 0, y: 0, width: 1000, height: 650)
            try capture(host, name: "navigator-\(slot.rawValue)")
        }
    }

    private func capture<V: View>(_ host: NSHostingView<V>, name: String) throws {
        if host.appearance == nil {
            host.appearance = NSAppearance(named: .aqua)
        }
        for _ in 0..<3 {
            host.layoutSubtreeIfNeeded()
            RunLoop.current.run(until: Date().addingTimeInterval(0.02))
        }
        host.layoutSubtreeIfNeeded()
        let rep = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
        host.cacheDisplay(in: host.bounds, to: rep)
        let data = try XCTUnwrap(rep.representation(using: .png, properties: [:]))
        XCTAssertGreaterThan(data.count, 100)
        if let output = ProcessInfo.processInfo.environment["TEST_UNDECLARED_OUTPUTS_DIR"] {
            try data.write(to: URL(fileURLWithPath: output).appendingPathComponent(name + ".png"))
        }
    }

    private func presentation(kind: String, mergeable: String? = nil, deferred: Bool = false, ci: String? = nil) throws -> NavigatorRowPresentation {
        var task = NavigatorTestFixture.task(kind: kind)
        task.prMergeableState = mergeable
        task.conflictResolutionDeferred = deferred
        task.ciRequiredState = ci
        return NavigatorRowPresentation(
            row: NavigatorRow(task: task, projection: try XCTUnwrap(task.navigator?.root), matchingAskID: nil),
            project: nil, width: 360
        )
    }

    private func iconLegend() throws -> some View {
        let specs: [(name: String, kind: String, mergeable: String?, deferred: Bool, ci: String?)] = [
            ("Chore", "chore", nil, false, nil),
            ("Task", "task", nil, false, nil),
            ("Project task", "project_task", nil, false, nil),
            ("Revision", "revision", nil, false, nil),
            ("Design", "design", nil, false, nil),
            ("Followup", "followup", nil, false, nil),
            ("Merge conflict", "task", "conflicting", false, nil),
            ("Deferred conflict", "task", "conflicting", true, nil),
            ("CI failed", "task", nil, false, "fail"),
            ("Revision with conflict", "revision", "conflicting", false, nil),
        ]
        let rows: [NavigatorRow] = try specs.map { spec in
            var task = NavigatorTestFixture.task(kind: spec.kind)
            task.name = spec.name
            task.prMergeableState = spec.mergeable
            task.conflictResolutionDeferred = spec.deferred
            task.ciRequiredState = spec.ci
            return NavigatorRow(task: task, projection: try XCTUnwrap(task.navigator?.root), matchingAskID: nil)
        }
        return VStack(alignment: .leading, spacing: 4) {
            ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                WorkNavigatorRow(row: row, project: "Navigation", width: 360, isSelected: false, isWorkerActive: false)
            }
        }
    }
}

private struct NavigatorSlotProbe: View {
    @State private var identity = UUID()
    let appeared: (UUID) -> Void
    var body: some View { Text("Registered body").onAppear { appeared(identity) } }
}
