import XCTest
@testable import Boss

final class WorkTreeDeltaCacheTests: XCTestCase {
    func testAttentionDeltasRetainUnchangedRowsAndRemoveDismissedGroups() throws {
        var cache = WorkTreeDeltaCache()
        let fields = ["attention_items", "attention_groups", "attention_members"]
        var initial: [String: Any] = ["product": ["id": "product"]]
        for field in fields { initial[field] = [["id": "old"], ["id": "kept"]] }
        XCTAssertNotNil(cache.apply(["product_id": "product", "sequence": UInt64(1), "changes": initial]))
        var changes: [String: Any] = [:]
        for field in fields {
            changes[field] = ["upserts": [["id": "new"]], "order": ["kept", "new"]]
        }
        let tree = try XCTUnwrap(cache.apply(["product_id": "product", "sequence": UInt64(2),
                                             "base_sequence": UInt64(1), "changes": changes]))
        for field in fields {
            XCTAssertEqual((tree[field] as? [[String: Any]])?.compactMap { $0["id"] as? String }, ["kept", "new"])
        }
    }

    func testSnapshotPatchDeletionReorderAndGapRecovery() throws {
        var cache = WorkTreeDeltaCache()
        cache.startConnection(1)
        let snapshot: [String: Any] = [
            "product_id": "product", "sequence": UInt64(1),
            "changes": ["product": ["id": "product"], "tasks": [
                ["id": "root", "badge": "pending"], ["id": "revision"], ["id": "other"]
            ], "task_runtimes": [["work_item_id": "revision"]], "dependencies": []]
        ]
        XCTAssertNotNil(cache.apply(snapshot))
        cache.startConnection(1) // Another frame on the same connection retains the base.
        let patch: [String: Any] = [
            "product_id": "product", "sequence": UInt64(2), "base_sequence": UInt64(1),
            "changes": [
                "tasks": ["upserts": [["id": "root", "badge": "clean"]], "order": ["other", "root"]],
                "task_runtimes": ["upserts": [], "order": []],
                "dependencies": ["replace": [["dependent_id": "root", "prerequisite_id": "other"]]]
            ]
        ]
        let tree = try XCTUnwrap(cache.apply(patch))
        let tasks = try XCTUnwrap(tree["tasks"] as? [[String: Any]])
        XCTAssertEqual(tasks.compactMap { $0["id"] as? String }, ["other", "root"])
        XCTAssertEqual(tasks[1]["badge"] as? String, "clean")
        XCTAssertEqual((tree["task_runtimes"] as? [[String: Any]])?.count, 0)
        XCTAssertNil(cache.apply(patch), "Duplicate or missing bases must trigger full recovery")
        XCTAssertNotNil(cache.apply(snapshot), "Reconnect snapshots replace stale session state")
        var wrongProduct = patch
        wrongProduct["product_id"] = "different"
        XCTAssertNil(cache.apply(wrongProduct))
        XCTAssertNotNil(cache.apply(patch), "Rejected patches must not corrupt the base")
        XCTAssertNotNil(cache.apply(snapshot)) // Old connection's base sequence is one again.
        cache.startConnection(2)
        XCTAssertNil(cache.apply(patch), "A lost reconnect snapshot must not match the old connection's sequence")
        XCTAssertNotNil(cache.apply(snapshot))
        XCTAssertNotNil(cache.apply(patch))
        cache.resetBase()
        XCTAssertNotNil(cache.apply(snapshot))
        cache.startConnection(2)
        XCTAssertNotNil(cache.apply(patch), "Full recovery must retain the connection generation")
    }
}
