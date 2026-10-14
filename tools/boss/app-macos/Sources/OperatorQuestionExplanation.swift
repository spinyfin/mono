import Foundation

/// Presentation convention within the existing explanation string, not a wire
/// format. Old or incomplete explanations remain visible verbatim.
struct OperatorQuestionExplanation: Equatable {
    let context: String
    let yes: String
    let no: String
    let recommendation: String
    let details: String?

    init?(_ explanation: String) {
        let lines = explanation.components(separatedBy: .newlines)
        let headings = ["Context:", "Yes:", "No:", "Recommendation:", "Details:"]
        var parts: [String] = []
        var body: [String] = []
        var next = 0
        for line in lines {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if next < headings.count, trimmed == headings[next] {
                if next == 0 {
                    guard body.allSatisfy({ $0.trimmingCharacters(in: .whitespaces).isEmpty }) else {
                        return nil
                    }
                } else {
                    parts.append(body.joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines))
                }
                body = []
                next += 1
            } else {
                // Reject malformed labels before Details; never discard or
                // interpret technical content inside Details.
                if next < headings.count, headings.contains(trimmed) { return nil }
                body.append(line)
            }
        }
        guard next >= 4 else { return nil }
        parts.append(body.joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines))
        guard parts.prefix(4).allSatisfy({ !$0.isEmpty }) else { return nil }
        context = parts[0]
        yes = parts[1]
        no = parts[2]
        recommendation = parts[3]
        details = parts.count == 5 ? parts[4] : nil
    }
}
