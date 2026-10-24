import SwiftUI

/// Shared by the card and its Why popover so both put the decision first.
struct OperatorQuestionDecisionContent: View {
    let question: String
    let explanation: OperatorQuestionExplanation

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(explanation.context)
            Text(question)
                .fontWeight(.medium)
                .accessibilityIdentifier("operator-question-text")
            if let yes = explanation.yes { labeled("Yes", yes) }
            if let no = explanation.no { labeled("No", no) }
            labeled("Recommendation", explanation.recommendation)
        }
        .font(.callout)
        .textSelection(.enabled)
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func labeled(_ label: String, _ text: String) -> some View {
        Text("\(Text("\(label): ").bold())\(text)")
    }
}
