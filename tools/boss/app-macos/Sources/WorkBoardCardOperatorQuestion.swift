import SwiftUI

// ===========================================================================
// The inline question on a Doing ▸ Needs Attention card: what a blocked
// worker asked the user, a "Why?" disclosure with the full detail, and
// Yes / No buttons.
//
// Equatable over its own [[OperatorQuestionPresentation]] so a live-status
// flip elsewhere on the card does not re-lay-out the block. The answer
// closure stays outside `==`, like every other card action.
// ===========================================================================

/// Inputs the question block paints. `nil` for every card that is not
/// awaiting an answer, so ordinary cards pay nothing.
struct WorkBoardCardOperatorQuestionSlice: Equatable {
    let presentation: OperatorQuestionPresentation

    init(presentation: OperatorQuestionPresentation) {
        self.presentation = presentation
    }

    init?(snapshot: WorkCardSnapshot) {
        guard let presentation = snapshot.operatorQuestion else { return nil }
        self.init(presentation: presentation)
    }
}

struct WorkBoardCardOperatorQuestion: View, @MainActor Equatable {
    let slice: WorkBoardCardOperatorQuestionSlice
    /// Invoked with the user's answer. The card never applies it
    /// locally: the engine's `work_item_updated` reply moves the card.
    var onAnswer: ((OperatorAnswer) -> Void)? = nil

    @State private var showingDetail = false

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.slice == rhs.slice
    }

    var body: some View {
        let question = slice.presentation
        VStack(alignment: .leading, spacing: 6) {
            // Three lines inline; the whole text is in the tooltip and the
            // "Why?" popover. Validation caps a question at 500 characters
            // so the inline rendering is the common case.
            Text(question.text)
                .font(.callout.weight(.medium))
                .foregroundStyle(.primary)
                .multilineTextAlignment(.leading)
                .lineLimit(OperatorQuestionPresentation.inlineLineLimit)
                .truncationMode(.tail)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .help(question.text)
                .accessibilityIdentifier("operator-question-text")

            HStack(spacing: 8) {
                Button("Why?") { showingDetail.toggle() }
                    .buttonStyle(.link)
                    .controlSize(.small)
                    .popover(isPresented: $showingDetail, arrowEdge: .bottom) {
                        OperatorQuestionDetailPopover(question: question)
                    }
                    .accessibilityIdentifier("operator-question-why")
                Spacer(minLength: 0)
                // No default action on either button: a stray Return must not
                // authorize a bypass.
                Button("Yes") { onAnswer?(.yesNo(true)) }
                    .accessibilityIdentifier("operator-question-yes")
                Button("No") { onAnswer?(.yesNo(false)) }
                    .accessibilityIdentifier("operator-question-no")
            }
            .controlSize(.small)
            .disabled(question.answerInFlight)

            if let error = question.errorMessage {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("operator-question-error")
            }
        }
    }
}

/// "Why?" popover: the whole question, the proposed task (for a prerequisite
/// question), why the worker asked, the run summary, and when — scrollable and
/// never truncated.
struct OperatorQuestionDetailPopover: View {
    let question: OperatorQuestionPresentation

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                section("Question", question.text)
                if let proposed = question.prerequisiteTask {
                    section("Proposed task", proposed.name)
                    section("Proposed brief", proposed.brief)
                }
                if !question.explanation.isEmpty {
                    section("Why the worker is asking", question.explanation)
                }
                section("Run summary", question.runSummary ?? "No run summary recorded.")
                if let asked = Self.askedLabel(question.askedAt) {
                    section("Asked", asked)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(12)
        }
        .frame(width: 340)
        .frame(maxHeight: 360)
    }

    static func askedLabel(_ raw: String) -> String? {
        guard let absolute = AutomationTime.absolute(raw) else { return nil }
        return "\(absolute) (\(AutomationTime.relative(raw, now: Date())))"
    }

    private func section(_ title: String, _ body: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .textCase(.uppercase)
            Text(body)
                .font(.callout)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}
