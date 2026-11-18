import Foundation

/// Lives on EngineClient's serial decode queue. The UI receives the reconciled
/// authoritative tree through its existing apply path, including cache eviction.
struct WorkTreeDeltaCache {
    private var sequence: UInt64?
    private var tree: [String: Any]?
    private var connectionGeneration: Int?

    mutating func startConnection(_ generation: Int) {
        guard connectionGeneration != generation else { return }
        connectionGeneration = generation
        resetBase()
    }

    mutating func resetBase() {
        sequence = nil
        tree = nil
    }

    /// The field each keyed collection is merged on; mirrors
    /// `boss_protocol::work_tree_delta`. Every other keyed field uses `id`.
    private static func recordKey(forField field: String) -> String {
        switch field {
        case "task_runtimes": return "work_item_id"
        case "planner_runs": return "project_id"
        default: return "id"
        }
    }

    mutating func apply(_ update: [String: Any]) -> [String: Any]? {
        guard let nextSequence = update["sequence"] as? UInt64,
              let productID = update["product_id"] as? String,
              let changes = update["changes"] as? [String: Any] else { return nil }
        if let base = update["base_sequence"] as? UInt64 {
            guard sequence == base, var next = tree,
                  (next["product"] as? [String: Any])?["id"] as? String == productID else { return nil }
            for (field, rawPatch) in changes {
                guard let patch = rawPatch as? [String: Any] else { return nil }
                if let replacement = patch["replace"] {
                    next[field] = replacement
                    continue
                }
                let key = Self.recordKey(forField: field)
                guard let before = next[field] as? [[String: Any]],
                      let upserts = patch["upserts"] as? [[String: Any]] else { return nil }
                var records: [String: [String: Any]] = [:]
                var oldOrder: [String] = []
                for row in before {
                    guard let id = row[key] as? String, records[id] == nil else { return nil }
                    records[id] = row
                    oldOrder.append(id)
                }
                for row in upserts {
                    guard let id = row[key] as? String else { return nil }
                    records[id] = row
                }
                let order = patch["order"] as? [String] ?? oldOrder
                guard Set(order).count == order.count else { return nil }
                var rows: [[String: Any]] = []
                for id in order {
                    guard let row = records[id] else { return nil }
                    rows.append(row)
                }
                next[field] = rows
            }
            tree = next
        } else {
            guard (changes["product"] as? [String: Any])?["id"] as? String == productID else { return nil }
            tree = changes
        }
        sequence = nextSequence
        return tree
    }
}
