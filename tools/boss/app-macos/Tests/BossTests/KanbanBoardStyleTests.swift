import XCTest

@testable import Boss

final class KanbanBoardStyleTests: XCTestCase {
    private var suiteName: String!
    private var defaults: UserDefaults!

    override func setUp() {
        super.setUp()
        suiteName = "dev.spinyfin.boss.tests.kanban-board-style.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suiteName)
        defaults.removePersistentDomain(forName: suiteName)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suiteName)
        defaults = nil
        suiteName = nil
        super.tearDown()
    }

    func testProductDefaultIsElevated() {
        XCTAssertEqual(KanbanBoardStyle.productDefault, .elevated)
    }

    func testClassicRawValueStaysClassic() {
        XCTAssertEqual(KanbanBoardStyle.classic.rawValue, "classic")
        XCTAssertEqual(KanbanBoardStyle(rawValue: "classic"), .classic)
    }

    func testClassicDisplayNameIsLegacy() {
        XCTAssertEqual(KanbanBoardStyle.classic.displayName, "Legacy")
        XCTAssertEqual(KanbanBoardStyle.elevated.displayName, "Elevated")
        XCTAssertEqual(KanbanBoardStyle.airy.displayName, "Airy")
        XCTAssertEqual(KanbanBoardStyle.minimal.displayName, "Minimal")
    }

    func testMissingPreferenceResolvesToElevated() {
        XCTAssertNil(defaults.string(forKey: KanbanBoardStyle.storageKey))
        XCTAssertEqual(KanbanBoardStyle.resolved(from: defaults), .elevated)
    }

    func testStoredClassicIsNotRewrittenToDefault() {
        defaults.set(KanbanBoardStyle.classic.rawValue, forKey: KanbanBoardStyle.storageKey)
        XCTAssertEqual(KanbanBoardStyle.resolved(from: defaults), .classic)
        XCTAssertEqual(
            defaults.string(forKey: KanbanBoardStyle.storageKey),
            "classic",
            "resolving must not rewrite an explicit Classic/Legacy preference"
        )
    }

    func testStoredElevatedSurvives() {
        defaults.set(KanbanBoardStyle.elevated.rawValue, forKey: KanbanBoardStyle.storageKey)
        XCTAssertEqual(KanbanBoardStyle.resolved(from: defaults), .elevated)
    }

    func testUnrecognizedStoredValueFallsBackToProductDefault() {
        defaults.set("not-a-style", forKey: KanbanBoardStyle.storageKey)
        XCTAssertEqual(KanbanBoardStyle.resolved(from: defaults), .elevated)
    }

    func testSnapshotContextDefaultMatchesProductDefault() {
        let context = WorkCardSnapshotContext(column: .backlog)
        XCTAssertEqual(context.boardStyle, KanbanBoardStyle.productDefault)
    }
}
