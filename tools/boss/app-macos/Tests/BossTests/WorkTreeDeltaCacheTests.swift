import XCTest
@testable import Boss

extension WorkTreeDeltaCacheTests {
    func testSnapshotPagesPublishAtomicallyAndRejectGaps() throws {
        var cache = WorkTreeDeltaCache()
        func page(_ index: Int, _ fields: [String: Any]) -> [String: Any] {
            ["product_id": "product", "sequence": UInt64(1),
             "changes": ["snapshot_page": ["index": index, "count": 2, "fields": fields]]]
        }
        let first = page(0, ["product": ["id": "product"], "tasks": [["id": "a"]]])
        let last = page(1, ["tasks": [["id": "b"]], "chores": []])
        XCTAssertThrowsError(try cache.collectPage(last))
        XCTAssertNil(try cache.collectPage(first))
        let update = try XCTUnwrap(cache.collectPage(last))
        let tree = try XCTUnwrap(cache.apply(update))
        XCTAssertEqual((tree["tasks"] as? [[String: Any]])?.compactMap { $0["id"] as? String }, ["a", "b"])
        XCTAssertNil(try cache.collectPage(first))
        cache.startConnection(2)
        XCTAssertThrowsError(try cache.collectPage(last), "pages must not survive reconnect")
    }
}

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

    func testPlannerRunDeltasMergeOnProjectIDAndTombstoneUndoneRuns() throws {
        var cache = WorkTreeDeltaCache()
        func summary(_ project: String, _ outcome: String) -> [String: Any] {
            ["run_id": "run_\(project)", "project_id": project, "outcome": outcome]
        }
        XCTAssertNotNil(cache.apply([
            "product_id": "product", "sequence": UInt64(1),
            "changes": ["product": ["id": "product"], "planner_runs": [summary("a", "staged"), summary("b", "staged")]]
        ]))
        let released = try XCTUnwrap(cache.apply([
            "product_id": "product", "sequence": UInt64(2), "base_sequence": UInt64(1),
            "changes": ["planner_runs": ["upserts": [summary("a", "applied")]]]
        ]))
        let rows = try XCTUnwrap(released["planner_runs"] as? [[String: Any]])
        XCTAssertEqual(rows.compactMap { $0["project_id"] as? String }, ["a", "b"])
        XCTAssertEqual(rows.first?["outcome"] as? String, "applied")
        XCTAssertEqual(rows.last?["outcome"] as? String, "staged", "an untouched project's row is retained")

        let undone = try XCTUnwrap(cache.apply([
            "product_id": "product", "sequence": UInt64(3), "base_sequence": UInt64(2),
            "changes": ["planner_runs": ["upserts": [], "order": ["b"]]]
        ]))
        XCTAssertEqual((undone["planner_runs"] as? [[String: Any]])?.compactMap { $0["project_id"] as? String }, ["b"])
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
