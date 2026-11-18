import SwiftUI

struct CriticalPriorityRequest: Identifiable {
    let id: String
    let ask: String
    let items: [String]
    let expiresAt: Date

    init?(_ raw: [String: Any]) {
        guard let id = raw["id"] as? String,
              let ask = raw["ask"] as? String,
              let items = raw["items"] as? [[String: Any]],
              let expires = raw["expires_at"] as? TimeInterval else { return nil }
        let labels = items.compactMap { item -> String? in
            guard let name = item["name"] as? String, let id = item["id"] as? String else { return nil }
            return "\(name) (\(id))"
        }
        guard !labels.isEmpty, labels.count == items.count else { return nil }
        self.id = id
        self.ask = ask
        self.items = labels
        self.expiresAt = Date(timeIntervalSince1970: expires)
    }
}

/// Human decisions live in the app chrome, never in the attention queue.
struct CriticalPriorityBanner: View {
    @ObservedObject var model: ChatViewModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(model.criticalPriorityRequests.filter { $0.expiresAt > Date() }.prefix(1)) { request in
                VStack(alignment: .leading, spacing: 6) {
                    Text("Set these items to critical?").font(.headline)
                    Text("Critical work runs ahead of other work and bypasses dispatch pause and concurrency limits.")
                    ScrollView {
                        VStack(alignment: .leading, spacing: 8) {
                            Text(request.ask).textSelection(.enabled)
                            Text(request.items.joined(separator: "\n")).textSelection(.enabled)
                        }.frame(maxWidth: .infinity, alignment: .leading)
                    }.frame(maxHeight: 140)
                    HStack {
                        Button("Confirm critical") { resolve(request, confirm: true) }
                        Button("Deny") { resolve(request, confirm: false) }
                        Text("Expires \(request.expiresAt, style: .relative)").font(.caption)
                    }
                    if model.criticalPriorityRequests.count > 1 {
                        Text("\(model.criticalPriorityRequests.count - 1) more requests waiting").font(.caption)
                    }
                }
                .padding(8)
                .background(Color.orange.opacity(0.12))
            }
        }
    }

    private func resolve(_ request: CriticalPriorityRequest, confirm: Bool) {
        model.engine.sendLine([
            "type": "resolve_critical_priority_request", "id": request.id, "confirm": confirm
        ])
    }
}
