import AppKit
import SwiftUI
import XCTest
@testable import Boss

@MainActor
final class WorkSwitcherTests: XCTestCase {
    private func withDefaults(_ body: (UserDefaults) -> Void) {
        let name = "work-switcher-tests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: name)!
        defer { defaults.removePersistentDomain(forName: name) }
        body(defaults)
    }

    private func model(_ defaults: UserDefaults) -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock", defaults: defaults)
    }

    private func product(_ id: String) -> WorkProduct {
        WorkProduct(id: id, name: id, slug: id, description: "", repoRemoteURL: nil,
                    status: "active", createdAt: "", updatedAt: "")
    }

    private func project(_ id: String, product: String = "Alpha") -> WorkProject {
        WorkProject(id: id, productID: product, name: id, slug: id, description: "", goal: "",
                    status: "active", priority: "medium", createdAt: "", updatedAt: "")
    }

    func testRecentOrderingFallbackLimitAndActiveItem() {
        withDefaults { defaults in
            let model = model(defaults)
            model.products = (0..<10).reversed().map { product("Product \($0)") }
            XCTAssertEqual(model.recentWorkProducts.map(\.id), (0..<7).map { "Product \($0)" })
            model.selectWorkProduct("Product 8")
            model.selectWorkProduct("Product 7")
            model.selectWorkProduct("Product 8")
            XCTAssertEqual(Array(model.recentWorkProducts.prefix(2).map(\.id)), ["Product 8", "Product 7"])
            XCTAssertEqual(model.recentWorkProducts.count, 7)

            var history = WorkSelectionHistory(defaults: defaults)
            for index in 0..<10 { history.useProduct("Product \(index)") }
            let recent = history.products(fallback: model.products.map(\.id), selected: "Product 0")
            XCTAssertEqual(recent.count, 7)
            XCTAssertTrue(recent.contains("Product 0"))
            XCTAssertEqual(recent.first, "Product 9")
        }
    }

    func testHistoryAndSidebarVisibilitySurviveNewModelAndDefaultsInstances() {
        let name = "work-switcher-relaunch.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: name)!
        defer { defaults.removePersistentDomain(forName: name) }
        let first = model(defaults)
        first.products = [product("Alpha"), product("Beta")]
        first.projectsByProductID["Alpha"] = [project("One"), project("Two")]
        first.selectWorkProduct("Beta")
        first.selectWorkProduct("Alpha")
        first.toggleProjectFilter("Two")
        first.toggleProjectFilter("One")
        first.isWorkSidebarHidden = true

        let relaunched = model(UserDefaults(suiteName: name)!)
        // Histories must survive the period before work trees arrive.
        relaunched.products = first.products
        relaunched.projectsByProductID = first.projectsByProductID
        XCTAssertEqual(relaunched.recentWorkProducts.map(\.id), ["Alpha", "Beta"])
        XCTAssertEqual(relaunched.recentWorkProjects.map(\.id), ["One", "Two"])
        XCTAssertEqual(relaunched.selectedProjectFilterIDs, ["One", "Two"])
        XCTAssertTrue(relaunched.isWorkSidebarHidden)
        relaunched.isWorkSidebarHidden = false
        XCTAssertFalse(model(UserDefaults(suiteName: name)!).isWorkSidebarHidden)
    }

    func testDeletedAndArchivedItemsArePrunedDurably() {
        withDefaults { defaults in
            let model = model(defaults)
            model.products = [product("Alpha"), product("Beta"), product("Gamma")]
            model.selectWorkProduct("Beta")
            model.selectWorkProduct("Gamma")
            model.selectWorkProduct("Alpha")
            model.projectsByProductID["Alpha"] = [project("One"), project("Two"), project("Three")]
            model.toggleProjectFilter("Two")
            model.toggleProjectFilter("Three")
            model.products.removeAll { $0.id == "Beta" }
            model.products[1].status = "archived"
            model.applyEventForTest(.productsList(products: model.products))
            model.projectsByProductID["Alpha"]?.removeAll { $0.id == "Two" }
            model.projectsByProductID["Alpha"]?[1].status = "archived"
            model.applyEventForTest(.projectsList(productId: "Alpha", projects: model.projectsByProductID["Alpha"]!))
            XCTAssertEqual(model.recentWorkProducts.map(\.id), ["Alpha"])
            XCTAssertEqual(model.recentWorkProjects.map(\.id), ["One"])
            // Reintroduced IDs must use fallback order, not their deleted history.
            let restored = WorkSelectionHistory(defaults: defaults)
            XCTAssertEqual(restored.products(fallback: ["Alpha", "Beta", "Gamma"], selected: nil), ["Alpha", "Beta", "Gamma"])
            XCTAssertEqual(restored.projects(productID: "Alpha", fallback: ["One", "Three", "Two"], selected: []), ["One", "Three", "Two"])
        }
    }

    func testSidebarAndDropdownShareSelectionCommandsInBothDirections() {
        withDefaults { defaults in
            let model = model(defaults)
            model.products = [product("Alpha"), product("Beta")]
            model.projectsByProductID["Alpha"] = [project("One"), project("Two")]
            model.projectsByProductID["Beta"] = [project("Other", product: "Beta")]
            model.selectWorkProduct("Alpha") // Sidebar selection.
            model.toggleProjectFilter("Two")
            XCTAssertEqual(model.workSwitcherLabel, "Alpha · Two")
            XCTAssertEqual(model.recentWorkProjects.first?.id, "Two")
            WorkSwitcherSelection.project("One").apply(to: model)
            XCTAssertEqual(model.selectedProjectFilterIDs, ["One", "Two"])
            XCTAssertEqual(model.workSwitcherLabel, "Alpha · 2 projects")
            WorkSwitcherSelection.allProjects.apply(to: model)
            XCTAssertFalse(model.hasProjectFilters)
            WorkSwitcherSelection.product("Beta").apply(to: model)
            XCTAssertEqual(model.selectedWorkProductID, "Beta")
            XCTAssertEqual(model.recentWorkProjects.map(\.id), ["Other"])
            // Card reveal and other direct changes also feed history.
            model.selectedWorkProductID = "Alpha"
            model.selectedProjectFilterIDs = ["Two"]
            XCTAssertEqual(model.recentWorkProducts.first?.id, "Alpha")
            XCTAssertEqual(model.recentWorkProjects.first?.id, "Two")
        }
    }

    func testPartialUpdatesDoNotEraseUnloadedHistory() {
        withDefaults { defaults in
            var history = WorkSelectionHistory(defaults: defaults)
            history.useProduct("Gamma")
            history.useProduct("Beta")
            history.useProjects(["Two"], productID: "Beta")
            history.useProjects(["Three"], productID: "Beta")
            let model = model(defaults)
            model.upsertProduct(product("Alpha"))
            model.upsertProject(project("One", product: "Beta"))
            let restored = WorkSelectionHistory(defaults: defaults)
            XCTAssertEqual(restored.products(fallback: ["Alpha", "Beta", "Gamma"], selected: nil), ["Beta", "Gamma", "Alpha"])
            XCTAssertEqual(restored.projects(productID: "Beta", fallback: ["One", "Two", "Three"], selected: []), ["Three", "Two", "One"])
            var archived = product("Beta")
            archived.status = "archived"
            model.upsertProduct(archived)
            let pruned = WorkSelectionHistory(defaults: defaults)
            XCTAssertEqual(pruned.products(fallback: ["Alpha", "Beta", "Gamma"], selected: nil), ["Gamma", "Alpha", "Beta"])
            XCTAssertEqual(pruned.projects(productID: "Beta", fallback: ["One", "Two", "Three"], selected: []), ["One", "Two", "Three"])
        }
    }

    func testSwitcherVisibilityFollowsSidebarAndWorkMode() {
        withDefaults { defaults in
            let model = model(defaults)
            model.navigationMode = .work
            model.isWorkSidebarHidden = false
            XCTAssertFalse(WorkSwitcherToolbar.isVisible(model: model))
            model.isWorkSidebarHidden = true
            XCTAssertTrue(WorkSwitcherToolbar.isVisible(model: model))
            model.navigationMode = .designs
            XCTAssertFalse(WorkSwitcherToolbar.isVisible(model: model))
            model.navigationMode = .work
            XCTAssertTrue(WorkSwitcherToolbar.isVisible(model: model))
        }
    }

    func testArrowKeysAndReturnSelectFromRealMenu() {
        withDefaults { defaults in
            let model = model(defaults)
            model.products = [product("Alpha"), product("Beta")]
            model.selectWorkProduct("Alpha")
            var dismissed = false
            var requests: [[String: Any]] = []
            model.engine.outboundRecorder = { requests.append($0) }
            let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 320, height: 300),
                                  styleMask: [.titled], backing: .buffered, defer: false)
            let host = NSHostingView(rootView: WorkSwitcherMenu(model: model) { dismissed = true })
            window.contentView = host
            for _ in 0..<5 {
                host.layoutSubtreeIfNeeded()
                RunLoop.current.run(until: Date().addingTimeInterval(0.02))
            }
            @MainActor func focusable(_ view: NSView) -> NSView? {
                for child in view.subviews {
                    if let found = focusable(child) { return found }
                }
                return view.acceptsFirstResponder ? view : nil
            }
            let target = focusable(host)
            XCTAssertNotNil(target)
            XCTAssertTrue(window.firstResponder === target, "Opening the popup must install keyboard focus")
            XCTAssertTrue(requests.isEmpty, "Opening the menu must only read local state")
            @MainActor func key(_ characters: String, code: UInt16) {
                let event = NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [],
                    timestamp: 0, windowNumber: window.windowNumber, context: nil,
                    characters: characters, charactersIgnoringModifiers: characters,
                    isARepeat: false, keyCode: code)!
                window.firstResponder?.keyDown(with: event)
                RunLoop.current.run(until: Date().addingTimeInterval(0.02))
            }
            key("\u{F701}", code: 125)
            key("\u{F700}", code: 126)
            key("\u{F701}", code: 125)
            key("\r", code: 36)
            XCTAssertEqual(model.selectedWorkProductID, "Beta")
            XCTAssertTrue(dismissed)
            XCTAssertFalse(window.isVisible)
        }
    }
}
