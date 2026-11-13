import Foundation

struct DispatchWaitBlocker: Hashable {
    let workItemID: String
    let productID: String
    let shortID: Int?
    /// "task" or "project"; selects the `T<n>` / `P<n>` short-id prefix.
    let kind: String

    init(workItemID: String, productID: String, shortID: Int?, kind: String = "task") {
        self.workItemID = workItemID
        self.productID = productID
        self.shortID = shortID
        self.kind = kind
    }

    init?(payload: [String: Any]) {
        guard let id = payload["work_item_id"] as? String,
              let product = payload["product_id"] as? String else { return nil }
        self.init(workItemID: id, productID: product, shortID: payload["short_id"] as? Int,
                  kind: payload["kind"] as? String ?? "task")
    }

    var label: String { shortID.map { "\(kind == "project" ? "P" : "T")\($0)" } ?? "a dependency" }
}

/// One recorded failure that fed a `failure_backoff` wait.
/// Mirrors `boss_protocol::DispatchFailureAttempt`.
struct DispatchFailureAttempt: Hashable {
    let errorText: String
    let happenedAt: String

    init(errorText: String, happenedAt: String) {
        self.errorText = errorText
        self.happenedAt = happenedAt
    }

    init?(payload: [String: Any]) {
        guard let raw = payload["error_text"] as? String else { return nil }
        let errorText = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !errorText.isEmpty else { return nil }
        self.init(errorText: errorText, happenedAt: payload["happened_at"] as? String ?? "")
    }
}
