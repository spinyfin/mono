import Foundation

/// Presentation convention within the existing explanation string, not a wire
/// format. Old or incomplete explanations remain visible verbatim.
struct OperatorQuestionExplanation: Equatable {
    let context: String
    /// Nil for a multiple-choice question, whose answers are option buttons
    /// rather than Yes/No.
    let yes: String?
    let no: String?
    let recommendation: String
    let details: String?

    private static let yesNoHeadings = ["Context:", "Yes:", "No:", "Recommendation:", "Details:"]
    private static let choiceHeadings = ["Context:", "Recommendation:", "Details:"]

    init?(_ explanation: String) {
        // The second label tells the two shapes apart.
        let isChoice = explanation.components(separatedBy: .newlines)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .first(where: { Self.yesNoHeadings.contains($0) && $0 != "Context:" }) == "Recommendation:"
        let headings = isChoice ? Self.choiceHeadings : Self.yesNoHeadings
        let required = headings.count - 1
        var parts: [String] = []
        var body: [String] = []
        var next = 0
        for line in explanation.components(separatedBy: .newlines) {
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
                if next < headings.count, Self.yesNoHeadings.contains(trimmed) { return nil }
                body.append(line)
            }
        }
        guard next >= required else { return nil }
        parts.append(body.joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines))
        guard parts.prefix(required).allSatisfy({ !$0.isEmpty }) else { return nil }
        context = parts[0]
        if isChoice {
            yes = nil
            no = nil
            recommendation = parts[1]
        } else {
            yes = parts[1]
            no = parts[2]
            recommendation = parts[3]
        }
        details = parts.count == headings.count ? parts[required] : nil
    }
}
