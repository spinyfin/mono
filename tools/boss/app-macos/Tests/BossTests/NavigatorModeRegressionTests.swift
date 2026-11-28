import XCTest
@testable import Boss

@MainActor
final class NavigatorModeRegressionTests: XCTestCase {
    func testHiddenPopoverCannotPresentOrDismissNavigatorSelection() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task()])
        model.selectWorkCard("root")
        XCTAssertTrue(model.isWorkCardPopoverPresented("root"))
        model.setWorkViewMode(.navigator)
        XCTAssertFalse(model.isWorkCardPopoverPresented("root"))
        model.dismissWorkCardPopover()
        XCTAssertEqual(model.selectedWorkCardID, "root")
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        XCTAssertEqual(model.selectionHistory.root(scope: model.navigatorScopeKey), "root")
    }

    func testKanbanScopeRoundTripDoesNotRestoreNavigatorHistory() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task(project: "alpha")])
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.toggleProjectFilter("alpha")
        model.setWorkViewMode(.kanban)
        model.clearProjectFilters()
        XCTAssertNil(model.selectedWorkCardID)
        model.toggleProjectFilter("alpha")
        XCTAssertNil(model.selectedWorkCardID)
    }

    func testRevealExpandsNavigatorSectionAndCompletes() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task(section: "landed")])
        model.setWorkViewMode(.navigator)
        let generation = model.navigator.revealGeneration
        var completed = false
        XCTAssertEqual(model.revealWorkCard("root", productID: "product") { _ in completed = true },
                       .revealed(cardID: "root"))
        XCTAssertEqual(model.navigator.mode, .navigator)
        XCTAssertFalse(model.navigatorCollapsed.contains(.landed))
        XCTAssertNotEqual(model.navigator.revealGeneration, generation)
        model.confirmNavigatorReveal(rootID: "root")
        XCTAssertTrue(completed)
    }

    func testAutomationRevealSwitchesToKanban() {
        var task = NavigatorTestFixture.task("automation")
        task.navigator = nil
        task.sourceAutomationId = "automation"
        let model = NavigatorTestFixture.model([task])
        model.setWorkViewMode(.navigator)
        XCTAssertEqual(model.revealWorkCard(task.id, productID: task.productID), .revealed(cardID: task.id))
        XCTAssertEqual(model.navigator.mode, .kanban)
        XCTAssertEqual(model.revealScrollTarget, task.id)
    }

    func testKanbanSwitchPreservesSearchAndNoProjectScopeWhenCardIsVisible() {
        let chore = NavigatorTestFixture.task(kind: "chore")
        let model = NavigatorTestFixture.model([chore])
        model.setWorkViewMode(.navigator)
        model.setFilterToChoresOnly(true)
        model.selectWorkCard("root")
        model.workSearchText = "navigation"
        XCTAssertTrue(model.filterToChoresOnly)
        XCTAssertTrue(model.navigatorScopeKey.contains("unprojected"))
        XCTAssertTrue(model.isTaskVisible(chore))
        XCTAssertEqual(model.navigator.selectedRootID, "root")
        model.setWorkViewMode(.kanban)
        XCTAssertTrue(model.filterToChoresOnly)
        XCTAssertEqual(model.workSearchText, "navigation")
        XCTAssertFalse(model.showBlockedOnly)
        XCTAssertTrue(model.includeChores)
        XCTAssertTrue(model.navigatorScopeKey.contains("unprojected"))
        XCTAssertEqual(model.selectedWorkCardID, "root")
        XCTAssertEqual(model.revealScrollTarget, "root")
        XCTAssertTrue(model.isTaskVisible(chore))
    }

    func testKanbanHandoffClearsFiltersThatHideRoot() {
        for filter in ["blocked", "chores", "search"] {
            let chore = NavigatorTestFixture.task(kind: "chore")
            let revision = NavigatorTestFixture.task("revision", rootID: chore.id)
            let model = NavigatorTestFixture.model([revision])
            model.choresByProductID["product"] = [chore]
            model.setWorkViewMode(.navigator)
            model.selectWorkCard("root")
            switch filter {
            case "blocked": model.showBlockedOnly = true
            case "chores": model.includeChores = false
            default: model.workSearchText = "revision"
            }
            XCTAssertFalse(model.isTaskVisible(chore), filter)
            XCTAssertEqual(model.navigatorSections[1].rows.map(\.id), ["root"], filter)
            model.setWorkViewMode(.kanban)
            XCTAssertFalse(model.showBlockedOnly, filter)
            XCTAssertTrue(model.includeChores, filter)
            XCTAssertEqual(model.workSearchText, "", filter)
            XCTAssertEqual(model.revealScrollTarget, "root", filter)
        }
    }

    func testRevealCompletionRejectsADifferentSelectedRoot() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task(), NavigatorTestFixture.task("other")])
        model.setWorkViewMode(.navigator)
        var completed = false
        model.revealWorkCard("root", productID: "product") { _ in completed = true }
        model.selectWorkCard("other")
        model.confirmNavigatorReveal(rootID: "other")
        XCTAssertFalse(completed)
        model.selectWorkCard("root")
        model.confirmNavigatorReveal(rootID: "root")
        XCTAssertTrue(completed)
        XCTAssertNil(model.navigatorRevealRootID)
    }

    func testSnapshotSynchronizesRestorationAndScopeExclusion() {
        let root = NavigatorTestFixture.task(project: "alpha")
        let model = NavigatorTestFixture.model([root])
        model.setWorkViewMode(.navigator)
        model.selectionHistory.rememberRoot(root.id, scope: model.navigatorScopeKey)
        NavigatorTestFixture.snapshot(model, [root])
        XCTAssertEqual(model.selectedWorkCardID, root.id)
        model.selectedProjectFilterIDs = ["beta"]
        NavigatorTestFixture.snapshot(model, [root])
        XCTAssertNil(model.selectedWorkCardID)
        XCTAssertNil(model.navigator.selectedRootID)
        model.setWorkViewMode(.kanban)
        model.setWorkViewMode(.navigator)
        XCTAssertNil(model.navigator.selectedRootID)
    }

    func testNavigatorCacheInvalidatesForSearchAndSnapshot() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task()])
        XCTAssertEqual(model.navigatorSections[1].rows.count, 1)
        XCTAssertNotNil(model.cachedNavigatorSections)
        XCTAssertNil(model.navigatorCompatibilityMessage)
        XCTAssertNotNil(model.cachedNavigatorCompatibility)
        model.workSearchText = "no match"
        XCTAssertNil(model.cachedNavigatorSections)
        XCTAssertTrue(model.navigatorSections[1].rows.isEmpty)
        model.workSearchText = ""
        NavigatorTestFixture.snapshot(model, [NavigatorTestFixture.task(section: "landed")])
        XCTAssertEqual(model.navigatorSections[4].rows.count, 1)
    }

    func testArchivedProjectFiltersUseTheSharedVisibleScope() {
        let model = NavigatorTestFixture.model([NavigatorTestFixture.task(project: "alpha")])
        model.selectedProjectFilterIDs = ["alpha"]
        var projects = model.projectsByProductID["product"] ?? []
        projects[0].status = "archived"
        model.projectsByProductID["product"] = projects
        model.showArchivedProjects = false
        XCTAssertTrue(model.visibleSelectedProjectFilterIDs.isEmpty)
        XCTAssertTrue(model.navigatorIncludes(NavigatorTestFixture.task(project: "beta")))
        XCTAssertEqual(model.navigatorScopeKey, "product")
        model.showArchivedProjects = true
        XCTAssertEqual(model.visibleSelectedProjectFilterIDs, ["alpha"])
        XCTAssertFalse(model.navigatorIncludes(NavigatorTestFixture.task(project: "beta")))
    }

    func testRootHistoryPrunesDeletedScopesAndSkipsUnchangedWrites() {
        let model = NavigatorTestFixture.model([])
        model.selectionHistory.rememberRoot("root", scope: "product|alpha")
        let saved = model.defaults.data(forKey: WorkSelectionHistory.storageKey)
        model.selectionHistory.rememberRoot("root", scope: "product|alpha")
        XCTAssertEqual(saved, model.defaults.data(forKey: WorkSelectionHistory.storageKey))
        model.selectionHistory.pruneProjects(productID: "product", validIDs: ["beta"])
        XCTAssertNil(model.selectionHistory.root(scope: "product|alpha"))
        model.selectionHistory.rememberRoot("root", scope: "product")
        model.selectionHistory.pruneProducts(validIDs: [])
        XCTAssertNil(model.selectionHistory.root(scope: "product"))
    }
}
