import Foundation

/// Presentation convention within the existing explanation string, not a wire
/// format. Old or incomplete explanations remain visible verbatim.
struct OperatorQuestionExplanation: Equatable {
    let context: String
    /// Nil for a multiple-choice question, whose answers are option buttons
    /// rather than Yes/No.
    let yes: String?
    let no: String?
    /// What the two sections are called: "Yes" / "No" unless the engine named
    /// the question's options (for example "Split" / "Bypass limit").
    let yesLabel: String
    let noLabel: String
    let recommendation: String
    let details: String?

    private static let choiceHeadings = ["Context:", "Recommendation:", "Details:"]

    /// `optionLabels` are the names the engine sent for a two-path question.
    /// An explanation written before the options were named still uses
    /// "Yes:" / "No:" sections, so those remain readable on such a question.
    init?(_ explanation: String, optionLabels: AnswerOptionLabels? = nil) {
        if let labels = optionLabels,
           let named = Self(parsing: explanation, yesLabel: labels.yes, noLabel: labels.no, allowsChoiceShape: false) {
            self = named
            return
        }
        self.init(parsing: explanation, yesLabel: "Yes", noLabel: "No", allowsChoiceShape: true)
    }

    /// `allowsChoiceShape` is false for a two-path question: its explanation
    /// always has both option sections, so a Context-then-Recommendation text
    /// is not a valid reading of it (it is an older Yes/No explanation, which
    /// the caller retries with the default headings).
    private init?(parsing explanation: String, yesLabel: String, noLabel: String, allowsChoiceShape: Bool) {
        let yesNoHeadings = ["Context:", "\(yesLabel):", "\(noLabel):", "Recommendation:", "Details:"]
        // The second label tells the two shapes apart.
        let isChoice = explanation.components(separatedBy: .newlines)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .first(where: { yesNoHeadings.contains($0) && $0 != "Context:" }) == "Recommendation:"
        if isChoice && !allowsChoiceShape { return nil }
        let headings = isChoice ? Self.choiceHeadings : yesNoHeadings
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
                if next < headings.count, yesNoHeadings.contains(trimmed) { return nil }
                body.append(line)
            }
        }
        guard next >= required else { return nil }
        parts.append(body.joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines))
        guard parts.prefix(required).allSatisfy({ !$0.isEmpty }) else { return nil }
        self.yesLabel = yesLabel
        self.noLabel = noLabel
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
