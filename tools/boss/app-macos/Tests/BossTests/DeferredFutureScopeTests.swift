import XCTest
@testable import Boss

/// Covers the durable `deferred` (future-scope) classification on the
/// kanban model: it decodes from the wire, defaults to `false` when absent
/// (legacy rows), and — being a chrome/badge concern only — does not alter
/// which column a card routes to.
@MainActor
final class DeferredFutureScopeTests: XCTestCase {

    private func basePayload(deferred: Any? = nil) -> [String: Any] {
        var payload: [String: Any] = [
            "id": "task_deferred_1",
            "product_id": "prod_test",
            "kind": "chore",
            "name": "Future work",
            "description": "",
            "status": "todo",
            "created_at": "2026-07-24T00:00:00Z",
            "updated_at": "2026-07-24T00:00:00Z",
        ]
        if let deferred {
            payload["deferred"] = deferred
        }
        return payload
    }

    func testDeferredDecodesTrue() {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let task = client.parseTask(basePayload(deferred: true))
        XCTAssertEqual(task?.deferred, true)
    }

    func testDeferredDefaultsFalseWhenAbsent() {
        let client = EngineClient(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let task = client.parseTask(basePayload())
        XCTAssertEqual(task?.deferred, false, "a legacy row without the field must decode as not-deferred")
    }

    /// The classification is a badge/chrome concern only: a deferred backlog
    /// card stays in Backlog exactly like any other `todo`, non-autostart row.
    func testDeferredDoesNotChangeBoardColumn() {
        var deferred = makeTask(deferred: true)
        deferred.autostart = false
        XCTAssertEqual(deferred.boardColumn, .backlog)

        var notDeferred = makeTask(deferred: false)
        notDeferred.autostart = false
        XCTAssertEqual(notDeferred.boardColumn, deferred.boardColumn)
    }

    private func makeTask(deferred: Bool) -> WorkTask {
        var task = WorkTask(
            id: "task_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Test item",
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-07-24T00:00:00Z",
            updatedAt: "2026-07-24T00:00:00Z"
        )
        task.deferred = deferred
        return task
    }
}
