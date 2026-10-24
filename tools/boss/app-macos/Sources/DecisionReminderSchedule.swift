import Foundation

struct DecisionNotification: Codable, Equatable {
    let questionID: String
    let text: String
    let workItemID: String
    let productID: String
    let label: String

    enum CodingKeys: String, CodingKey {
        case questionID = "question_id", text, workItemID = "work_item_id"
        case productID = "product_id", label
    }
}

/// Wall-clock schedule persisted across app restarts. Only open questions
/// survive reconciliation. A late wake sends one reminder, never a backlog.
struct DecisionReminderSchedule {
    static let intervalKey = "progression.remind_after"
    // Design placeholder; the progression rollout's measurement task sets this.
    static let defaultInterval: TimeInterval = 3 * 24 * 60 * 60
    static func interval(_ value: Double) -> TimeInterval {
        value.isFinite && value >= 1 ? value : defaultInterval
    }

    private(set) var questions: [String: DecisionNotification] = [:]
    var lastSent: [String: TimeInterval] = [:]

    mutating func reconcile(_ open: [DecisionNotification]) -> Set<String> {
        let next = Dictionary(open.map { ($0.questionID, $0) }, uniquingKeysWith: { _, newer in newer })
        let removed = Set(questions.keys).union(lastSent.keys).subtracting(next.keys)
        questions = next
        lastSent = lastSent.filter { next[$0.key] != nil && $0.value.isFinite }
        return removed
    }

    func due(at now: Date, interval: TimeInterval) -> [DecisionNotification] {
        questions.values.filter { question in
            guard let sent = lastSent[question.questionID] else { return true }
            return now.timeIntervalSince1970 >= sent + interval
        }.sorted { $0.questionID < $1.questionID }
    }

    func nextDate(interval: TimeInterval, excluding: Set<String> = []) -> Date? {
        questions.keys.filter { !excluding.contains($0) }.map { Date(timeIntervalSince1970: (lastSent[$0] ?? 0) + interval) }.min()
    }
}
