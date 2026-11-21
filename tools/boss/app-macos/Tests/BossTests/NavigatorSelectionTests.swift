import XCTest
@testable import Boss

@MainActor
final class NavigatorSelectionTests: XCTestCase {
    func testReadyIsTheFirstNavigatorSectionAndCountsLikeTheOthers() {
        XCTAssertEqual(
            NavigatorSection.allCases.map(\.title),
            ["Ready", "Working", "Needs attention", "Backlog", "Landed"]
        )
        let ready = NavigatorTestFixture.task("ready", section: "ready", order: "0")
        let working = NavigatorTestFixture.task("working", section: "working", order: "1")
        let model = NavigatorTestFixture.model([ready, working])
        XCTAssertEqual(model.navigatorSections.map(\.section), NavigatorSection.allCases)
        XCTAssertEqual(model.navigatorSections.map(\.total), [1, 1, 0, 0, 0])
        XCTAssertEqual(model.navigatorSections[0].rows.map(\.id), ["ready"])
        XCTAssertEqual(model.navigatorSections[1].rows.map(\.id), ["working"])
        model.setNavigatorSection(.ready, expanded: false)
        XCTAssertTrue(model.navigatorCollapsed.contains(.ready))
        XCTAssertEqual(model.navigatorVisibleRows.map(\.id), ["working"])
        model.setNavigatorSection(.ready, expanded: true)
        XCTAssertEqual(model.navigatorVisibleRows.map(\.id), ["ready", "working"])
    }

    func testExplicitDeselectionSurvivesRefreshAndExcludedCardDoesNotReuseOldRoot() {
        let root = NavigatorTestFixture.task()
        var excluded = NavigatorTestFixture.task("automation")
        excluded.navigator = nil
        excluded.sourceAutomationId = "automation"
        let model = NavigatorTestFixture.model([root, excluded])
        model.selectWorkCard("root")
        model.selectWorkCard(nil)
        NavigatorTestFixture.snapshot(model, [root, excluded])
        XCTAssertNil(model.navigator.selectedRootID)
        model.selectWorkCard("root")
        model.selectWorkCard("automation")
        model.setWorkViewMode(.navigator)
        XCTAssertNil(model.navigator.selectedRootID)
        XCTAssertNil(model.selectedWorkCardID)
        NavigatorTestFixture.snapshot(model, [root, excluded])
        XCTAssertNil(model.navigator.selectedRootID)
    }

    func testDeltaRecoveryKeepsRootSelectedAndRejectsOldConnectionBase() throws {
        func wire(_ section: String, detail: String) -> [String: Any] {
            ["id": "root", "product_id": "product", "kind": "task", "name": "Root", "description": "Brief",
             "status": "active", "created_at": "", "updated_at": "", "navigator": [
                "version": 1, "root_task_id": "root", "root": [
                    "section": section, "sort_key": "0", "default_detail": detail,
                    "reason": ["type": "future_reason"], "reason_label": "Engine reason",
                    "revisions": ["open": 0, "total": 0],
                ],
             ]]
        }
        var cache = WorkTreeDeltaCache()
        cache.startConnection(1)
        let model = NavigatorTestFixture.model([])
        func apply(_ tree: [String: Any]) throws {
            let rows = try XCTUnwrap(tree["tasks"] as? [[String: Any]])
            let tasks = rows.compactMap(model.engine.parseTask)
            XCTAssertEqual(tasks.count, rows.count)
            NavigatorTestFixture.snapshot(model, tasks)
        }
        let initial: [String: Any] = ["product_id": "product", "sequence": UInt64(1), "changes": [
            "product": ["id": "product"], "tasks": [wire("working", detail: "activity")],
        ]]
        try apply(XCTUnwrap(cache.apply(initial)))
        model.selectWorkCard("root")
        model.navigator.tab = .history
        let patch: [String: Any] = ["product_id": "product", "sequence": UInt64(2), "base_sequence": UInt64(1),
                                   "changes": ["tasks": ["upserts": [wire("needs_attention", detail: "attention")]]]]
        try apply(XCTUnwrap(cache.apply(patch)))
        XCTAssertEqual(model.navigatorSections.map(\.total), [0, 0, 1, 0, 0])
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        XCTAssertEqual(model.navigator.tab, .history)
        cache.startConnection(2)
        XCTAssertNil(cache.apply(patch))
        XCTAssertEqual(model.navigatorSelectedTask?.navigator?.root?.section, "needs_attention")
        try apply(XCTUnwrap(cache.apply(initial)))
        XCTAssertEqual(model.navigatorSelectedTask?.navigator?.root?.section, "working")
        XCTAssertEqual(model.navigator.tab, .history)
    }

    func testEngineOrderCountsAndRevisionSearchIgnoreKanbanFilters() {
        let root = NavigatorTestFixture.task(order: "z", project: "alpha")
        let other = NavigatorTestFixture.task("other", order: "a", project: "alpha")
        var revision = NavigatorTestFixture.task("revision", rootID: root.id)
        revision.description = "Repair keyboard routing"
        let model = NavigatorTestFixture.model([root, other, revision])
        model.showBlockedOnly = true
        model.includeChores = false
        model.toggleProjectFilter("alpha")
        XCTAssertEqual(model.navigatorSections[1].rows.map(\.id), ["other", "root"])
        model.workSearchText = "keyboard"
        XCTAssertEqual(model.navigatorSections[1].total, 2)
        XCTAssertEqual(model.navigatorSections[1].rows.map(\.id), ["root"])
        XCTAssertEqual(model.navigatorSections[1].rows.first?.matchingAskID, "revision")
        model.moveNavigatorSelection(by: 1)
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        XCTAssertEqual(model.navigator.askID, "revision")
    }

    func testSelectedRootMovesAtomicallyIntoCollapsedDestination() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task()])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.navigator.tab = .history
        let reveal = model.navigator.revealGeneration
        NavigatorTestFixture.snapshot(model, [NavigatorTestFixture.task(section: "landed", detail: "review")])
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        XCTAssertEqual(model.navigator.tab, .history)
        XCTAssertEqual(model.navigator.revealGeneration, reveal)
        XCTAssertEqual(model.navigatorSections.map(\.total), [0, 0, 0, 0, 1])
        XCTAssertTrue(model.navigatorVisibleRows.isEmpty)
        XCTAssertEqual(model.navigatorSelectionNotice, "Moved to Landed.")
        model.revealNavigatorSelection()
        XCTAssertEqual(model.navigatorVisibleRows.map(\.id), ["root"])
        XCTAssertNil(model.navigatorSelectionNotice)
    }

    func testModeSwitchResolvesRevisionAndRetainsDetailAndScopeState() {
        var revision = NavigatorTestFixture.task("revision", rootID: "root")
        revision.operatorQuestion = OperatorQuestion(
            id: "question", text: "Approve the bypass?", answerType: .yesNo,
            explanation: "Need a decision.", askedAt: "1790000000", executionID: "exec-question"
        )
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task(), revision])
        model.selectWorkCard("revision")
        model.setWorkViewMode(.navigator)
        XCTAssertEqual(model.selectedWorkCardID, "root", "Shared toolbar actions must target the selected root")
        model.navigator.tab = .attention
        model.navigator.scrollByScope[model.navigatorScopeKey] = "root"
        model.setNavigatorSection(.backlog, expanded: false)
        model.setWorkViewMode(.kanban)
        XCTAssertEqual(model.selectedWorkCardID, "root")
        XCTAssertEqual(model.revealScrollTarget, "root")
        model.setWorkViewMode(.navigator)
        XCTAssertEqual(model.navigator.tab, .attention)
        XCTAssertEqual(model.navigator.askID, "revision")
        XCTAssertEqual(model.navigator.scrollByScope[model.navigatorScopeKey], "root")
        XCTAssertTrue(model.navigatorCollapsed.contains(.backlog))
        XCTAssertEqual(model.defaults.string(forKey: "boss.work.viewMode"), "navigator")
    }

    func testScopeChangesPreserveIncludedSelectionAndRestoreExcludedRoot() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task(project: "alpha"),
                                                NavigatorTestFixture.task("other", project: "beta")])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.toggleProjectFilter("alpha")
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        model.changeNavigatorScope { model.selectedProjectFilterIDs = ["beta"] }
        XCTAssertNil(model.navigator.selectedRootID)
        model.selectWorkCard("other")
        model.changeNavigatorScope { model.selectedProjectFilterIDs = ["alpha"] }
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        model.clearProjectFilters()
        XCTAssertEqual(model.navigator.selectedRootID, "root")
    }

    func testDisconnectReconnectAndRemovalRetainSelection() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task()])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.navigator.tab = .history
        model.isConnected = false
        XCTAssertEqual(model.navigatorSelectedTask?.id, "root")
        model.isConnected = true
        NavigatorTestFixture.snapshot(model, [NavigatorTestFixture.task(section: "needs_attention")])
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        XCTAssertEqual(model.navigator.tab, .history)
        NavigatorTestFixture.snapshot(model, [])
        XCTAssertEqual(model.navigatorSelectedTask?.id, "root")
        XCTAssertTrue(model.navigatorSelectionNotice?.contains("removed") == true)
    }

    func testDeletedRootThenSnapshotKeepsRetainedDetail() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task()])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.navigator.tab = .history
        model.applyEventForTest(.workItemDeleted(id: "root"))
        XCTAssertNil(model.selectedWorkCardID)
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        XCTAssertEqual(model.navigator.retainedTask?.id, "root")
        NavigatorTestFixture.snapshot(model, [])
        XCTAssertEqual(model.selectedWorkCardID, "root")
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        XCTAssertEqual(model.navigatorSelectedTask?.id, "root")
        XCTAssertEqual(model.navigator.tab, .history)
        XCTAssertTrue(model.navigatorSelectionNotice?.contains("removed") == true)
    }

    func testClearingCurrentTaskIdClearsAskID() {
        let root = NavigatorTestFixture.task()
        let revision = NavigatorTestFixture.task("revision", rootID: "root")
        let model = NavigatorTestFixture.model([root, revision])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        XCTAssertEqual(model.navigator.askID, "revision")
        var cleared = root
        cleared.navigator = NavigatorProjection.parse([
            "version": 1, "root_task_id": "root", "root": [
                "section": "working", "sort_key": "0", "default_detail": "activity",
                "reason": ["type": "active", "activity": "revising"], "reason_label": "Revising",
                "revisions": ["open": 0, "total": 3],
                "activities": [["execution_id": "execution", "task_id": "revision", "run_id": "run"]],
                "actions": [["type": "attention", "kind": "question", "attention_id": "question", "task_id": "revision"]],
                "commands": [["kind": "open_pr", "task_id": "root"]],
            ] as [String: Any],
        ])
        model.setNavigatorDocument(.currentAsk, rootID: "root")
        NavigatorTestFixture.snapshot(model, [cleared, revision])
        XCTAssertNil(model.navigator.askID)
        XCTAssertEqual(model.navigatorDocument(for: "root"), .original)
        XCTAssertEqual(model.navigatorDocumentTask(for: model.navigatorSelectedTask!).id, "root")
    }

    func testUnloadedCurrentTaskIdKeepsPreviousAsk() {
        let root = NavigatorTestFixture.task()
        let revision = NavigatorTestFixture.task("revision", rootID: "root")
        let model = NavigatorTestFixture.model([root, revision])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        XCTAssertEqual(model.navigator.askID, "revision")
        var advanced = root
        advanced.navigator = NavigatorProjection.parse([
            "version": 1, "root_task_id": "root", "root": [
                "section": "working", "sort_key": "0", "default_detail": "activity",
                "reason": ["type": "active", "activity": "revising"], "reason_label": "Revising",
                "revisions": ["open": 1, "total": 4, "current_task_id": "next"],
                "activities": [["execution_id": "execution", "task_id": "revision", "run_id": "run"]],
                "actions": [["type": "attention", "kind": "question", "attention_id": "question", "task_id": "revision"]],
                "commands": [["kind": "open_pr", "task_id": "root"]],
            ] as [String: Any],
        ])
        NavigatorTestFixture.snapshot(model, [advanced, revision])
        XCTAssertEqual(model.navigator.askID, "revision")
    }

    func testRevealNonCurrentRevisionSetsAskFromEmptySelection() {
        let root = NavigatorTestFixture.task()
        let current = NavigatorTestFixture.task("revision", rootID: "root")
        let older = NavigatorTestFixture.task("older", rootID: "root")
        let model = NavigatorTestFixture.model([root, current, older])
        model.setWorkViewMode(.navigator)
        XCTAssertNil(model.navigator.selectedRootID)
        XCTAssertEqual(model.revealWorkCard("older", productID: "product"), .revealed(cardID: "root"))
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        XCTAssertEqual(model.navigator.askID, "revision")
        XCTAssertEqual(model.navigatorDocument(for: "root"), .historical)
        XCTAssertEqual(model.navigatorDocumentTask(for: root).id, "older")
    }

    func testRevealNonCurrentRevisionSetsAskWhenRootAlreadySelected() {
        let root = NavigatorTestFixture.task()
        let current = NavigatorTestFixture.task("revision", rootID: "root")
        let older = NavigatorTestFixture.task("older", rootID: "root")
        let model = NavigatorTestFixture.model([root, current, older])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        XCTAssertEqual(model.navigator.askID, "revision")
        XCTAssertEqual(model.revealWorkCard("older", productID: "product"), .revealed(cardID: "root"))
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        XCTAssertEqual(model.navigator.askID, "revision")
        XCTAssertEqual(model.navigatorDocument(for: "root"), .historical)
        XCTAssertEqual(model.navigatorDocumentTask(for: root).id, "older")
    }

    func testAcknowledgementDoesNotSpeculateAboutPlacement() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task()])
        var acknowledgement = NavigatorTestFixture.task(section: "landed")
        acknowledgement.navigator = nil
        acknowledgement.status = "done"
        model.applyIncrementalTaskUpdate(acknowledgement, isChore: false)
        XCTAssertEqual(model.navigatorSections.map(\.total), [0, 1, 0, 0, 0])
    }

    func testKeyboardSkipsCollapsedAndFilteredRowsAndSupportsEdges() {
        let model = NavigatorTestFixture.model([
            NavigatorTestFixture.task("first", order: "a"), NavigatorTestFixture.task("last", order: "z"),
            NavigatorTestFixture.task("hidden", section: "landed"),
        ])
        model.moveNavigatorSelection(by: 1)
        XCTAssertEqual(model.navigator.selectedRootID, "first")
        model.moveNavigatorSelection(by: 1, edge: true)
        XCTAssertEqual(model.navigator.selectedRootID, "last")
        model.workSearchText = "first"
        model.moveNavigatorSelection(by: -1, edge: true)
        XCTAssertEqual(model.navigator.selectedRootID, "first")
    }
}
