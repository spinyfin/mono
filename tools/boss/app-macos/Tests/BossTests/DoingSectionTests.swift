import Combine
import XCTest
@testable import Boss

@MainActor
final class DoingSectionTests: XCTestCase {
    private func item(_ id: String) -> WorkTask {
        WorkTask(
            id: id, productID: "product", projectID: nil, kind: "chore",
            name: id, description: "", status: "active", priority: "medium", ordinal: nil,
            prURL: nil, deletedAt: nil, createdAt: "2026-10-01", updatedAt: "2026-10-01"
        )
    }

    func testOrderCountsAndEmptySectionsUseSharedSectionShape() {
        let rows = [item("queued"), item("review"), item("code"), item("code2")]
        let sections = ChatViewModel.doingSections(items: rows, membership: [
            "review": .reviewing, "code": .coding, "code2": .coding,
        ])
        XCTAssertEqual(sections.map(\.title), ["Coding", "Reviewing", "Queued"])
        XCTAssertEqual(sections.map(\.id), ["doing-coding", "doing-reviewing", "doing-queued"])
        XCTAssertEqual(sections.map { $0.items.count }, [2, 1, 1])
        XCTAssertEqual(sections[0].items.map(\.id), ["code", "code2"])
        XCTAssertTrue(sections.allSatisfy { $0.isCollapsible && $0.defaultExpanded })
        XCTAssertTrue(ChatViewModel.doingSections(items: [], membership: [:]).isEmpty)
        XCTAssertEqual(ChatViewModel.doingSections(items: [item("code")], membership: ["code": .coding]).map(\.title), ["Coding"])
    }

    func testLiveSnapshotMovesCachedCardsAndOnlyPublishesMembershipChanges() {
        let model = ChatViewModel(socketPath: "/tmp/doing-test-\(UUID().uuidString).sock")
        model.selectedWorkProductID = "product"
        model.choresByProductID = ["product": [item("card")]]
        model.workBoardGrouping = .project
        XCTAssertEqual(model.workSections(in: .doing).map(\.title), ["Queued"])
        var publishes = 0
        let subscription = model.$doingSectionsByID.dropFirst().sink { _ in publishes += 1 }
        for section in [DoingSection.coding, .reviewing, .queued] {
            let membership: [String: DoingSection] = section == .queued ? [:] : ["card": section]
            model.applyEventForTest(.workerLiveStatesList(states: [], doingSections: membership))
            XCTAssertEqual(model.workSections(in: .doing).map(\.title), [section.title])
            XCTAssertEqual(model.workSections(in: .doing).flatMap(\.items).map(\.id), ["card"])
            model.applyEventForTest(.workerLiveStatesList(states: [], doingSections: membership))
        }
        XCTAssertEqual(publishes, 3, "tool events with unchanged membership must not rebuild the board")
        withExtendedLifetime(subscription) {}
    }

    func testCollapsePreferenceUsesTheExistingPersistentKey() {
        let id = "doing-coding"
        let key = WorkBoardSectionCollapse.storageKey(sectionID: id)
        let previous = BossDefaults.store.object(forKey: key)
        defer {
            if let previous { BossDefaults.store.set(previous, forKey: key) }
            else { BossDefaults.store.removeObject(forKey: key) }
        }
        BossDefaults.store.set(true, forKey: key)
        for _ in 0..<2 {
            let section = ChatViewModel.doingSections(items: [item("code")], membership: ["code": .coding])[0]
            XCTAssertFalse(WorkBoardSectionCollapse.isExpanded(sectionID: section.id, defaultExpanded: section.defaultExpanded))
        }
        BossDefaults.store.set(false, forKey: key)
        XCTAssertTrue(WorkBoardSectionCollapse.isExpanded(sectionID: id, defaultExpanded: true))
    }
}

final class DoingSectionWireTests: XCTestCase {
    func testEngineMembershipIsDecodedWithoutInferringFromStates() {
        let client = EngineClient(socketPath: "/tmp/doing-wire-\(UUID().uuidString).sock")
        let delivered = expectation(description: "board membership")
        client.onEvent = { event in
            guard case .workerLiveStatesList(let states, let sections) = event else {
                XCTFail("expected worker board snapshot")
                return
            }
            XCTAssertTrue(states.isEmpty)
            XCTAssertEqual(sections, ["code": .coding, "review": .reviewing, "queue": .queued])
            delivered.fulfill()
        }
        client.consumeLineForTesting("""
        {"payload":{"type":"worker_live_states_list","states":[],"doing_sections":{"code":"coding","review":"reviewing","queue":"queued"}}}
        """)
        wait(for: [delivered], timeout: 2)
    }
}
