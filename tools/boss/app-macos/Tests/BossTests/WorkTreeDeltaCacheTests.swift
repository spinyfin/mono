import XCTest
@testable import Boss

final class WorkTreeDeltaCacheTests: XCTestCase {
    func testSnapshotPatchDeletionReorderAndGapRecovery() throws {
        var cache = WorkTreeDeltaCache()
        let snapshot: [String: Any] = [
            "product_id": "product", "sequence": UInt64(1),
            "changes": ["product": ["id": "product"], "tasks": [
                ["id": "root", "badge": "pending"], ["id": "revision"], ["id": "other"]
            ], "task_runtimes": [["work_item_id": "revision"]], "dependencies": []]
        ]
        XCTAssertNotNil(cache.apply(snapshot))
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
    }
}
