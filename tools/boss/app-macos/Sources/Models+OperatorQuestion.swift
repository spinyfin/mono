import Foundation

/// The open question a blocked worker asked the user, as projected onto a
/// task by the engine (`Task.operator_question`, `OperatorQuestionView` on the
/// wire). `nil` on `WorkTask` once the question is answered or withdrawn.
///
/// The card renders `text` with labeled decision context from `explanation`;
/// technical details stay in the "Why?" popover. `askedAt` orders the Doing
/// column's "Needs Attention" section.
struct OperatorQuestion: Hashable {
    /// How the user answers. Mirrors `OperatorAnswerType`. The yes/no and
    /// prerequisite kinds are answered with Yes/No buttons, a multiple-choice
    /// question with one button per option; a payload carrying any other kind
    /// is not decoded (see [[parse(_:)]]) because the card has no UI to answer it.
    enum AnswerType: String, Hashable {
        case yesNo = "yes_no"
        /// One button per entry of [[OperatorQuestion.choices]]; the answer is
        /// the chosen string, verbatim.
        case multipleChoice = "multiple_choice"
        /// The worker proposes a task that must land before it can proceed;
        /// the proposal rides on [[OperatorQuestion.prerequisiteTask]].
        case createPrerequisiteTask = "create_prerequisite_task"
    }

    let id: String
    let text: String
    let answerType: AnswerType
    let explanation: String
    /// When the worker's run declared the question, as the engine's epoch-seconds string (`AutomationTime.parse` also accepts RFC 3339).
    let askedAt: String
    /// The run that asked; the answer restarts the task in a new run.
    let executionID: String
    var runSummary: String? = nil
    /// The proposed task, present exactly when `answerType` is
    /// `.createPrerequisiteTask`. The card shows it behind "Why?".
    var prerequisiteTask: PrerequisiteTaskProposal? = nil
    /// The options of a `.multipleChoice` question, in the worker's order;
    /// empty for every other answer type.
    var choices: [String] = []

    /// How many options the engine accepts on a multiple-choice question
    /// (`OperatorAnswerType::MIN_CHOICES` / `MAX_CHOICES`).
    static let choiceCountRange = 2...6

    /// Decode the wire `operator_question` object. Absent / null / malformed
    /// → `nil`, and so is a question whose `answer_type.kind` this build
    /// cannot render: a stored question the card cannot answer would be a
    /// permanent park, so such a task falls back to the plain blocked card
    /// in Backlog (its tooltip already carries the question text via
    /// `blocked_detail`) rather than a Needs Attention card with no buttons.
    static func parse(_ value: Any?) -> OperatorQuestion? {
        guard let dict = value as? [String: Any],
              let id = dict["id"] as? String,
              let text = dict["text"] as? String,
              let answerTypeDict = dict["answer_type"] as? [String: Any],
              let kind = answerTypeDict["kind"] as? String,
              let answerType = AnswerType(rawValue: kind),
              let askedAt = dict["asked_at"] as? String
        else { return nil }
        var prerequisiteTask: PrerequisiteTaskProposal? = nil
        if answerType == .createPrerequisiteTask {
            // A proposal with nothing to show behind "Why?" cannot be
            // displayed; fall back to the plain blocked card.
            guard let name = answerTypeDict["name"] as? String,
                  let brief = answerTypeDict["brief"] as? String
            else { return nil }
            prerequisiteTask = PrerequisiteTaskProposal(name: name, brief: brief)
        }
        var choices: [String] = []
        if answerType == .multipleChoice {
            // Options the card cannot render as distinct buttons make a
            // question nobody can answer; fall back to the plain blocked card.
            guard let wireChoices = answerTypeDict["choices"] as? [String],
                  choiceCountRange.contains(wireChoices.count),
                  // Compare scalars, as the engine compares bytes: String equality
                  // would merge canonically equivalent spellings the engine
                  // accepts as distinct.
                  Set(wireChoices.map { Array($0.unicodeScalars.map(\.value)) }).count == wireChoices.count,
                  !wireChoices.contains(where: { $0.isEmpty })
            else { return nil }
            choices = wireChoices
        }
        return OperatorQuestion(
            id: id,
            text: text,
            answerType: answerType,
            explanation: (dict["explanation"] as? String) ?? "",
            askedAt: askedAt,
            executionID: (dict["execution_id"] as? String) ?? "",
            runSummary: dict["run_summary"] as? String,
            prerequisiteTask: prerequisiteTask,
            choices: choices
        )
    }
}

/// The task a blocked worker proposes as its prerequisite
/// (`OperatorAnswerType::CreatePrerequisiteTask` on the wire).
struct PrerequisiteTaskProposal: Hashable {
    let name: String
    /// The proposed task's self-contained brief.
    let brief: String
}

/// The user's answer, shaped by the question's `AnswerType`. Encodes to
/// the wire `OperatorAnswer` tagged object.
enum OperatorAnswer: Equatable {
    case yesNo(Bool)
    /// The chosen option of a multiple-choice question, exactly as offered.
    case multipleChoice(String)

    var wirePayload: [String: Any] {
        switch self {
        case .yesNo(let value):
            return ["kind": "yes_no", "value": value]
        case .multipleChoice(let value):
            return ["kind": "multiple_choice", "value": value]
        }
    }
}

/// What the card needs to render the inline question, precomputed for
/// [[WorkCardSnapshot]] so the card stays a pure render of its snapshot.
struct OperatorQuestionPresentation: Equatable {
    let questionID: String
    let text: String
    let explanation: String
    let askedAt: String
    /// `true` while an answer for this question is awaiting the engine's
    /// reply; both buttons are disabled so a double click cannot send twice.
    let answerInFlight: Bool
    /// Transient refusal from the last answer attempt (`Conflict`,
    /// `NotFound`, …). The card stays in place until the next
    /// `WorkItemUpdated` moves it; this tells the user why a click did
    /// nothing.
    let errorMessage: String?
    var runSummary: String? = nil
    /// Set for a "create a prerequisite task" question; the details popover
    /// shows the proposed task's name and brief.
    var prerequisiteTask: PrerequisiteTaskProposal? = nil
    /// The options of a multiple-choice question, one button each; empty for
    /// a Yes/No question, which keeps its Yes / No buttons.
    var choices: [String] = []

    /// The inline question is capped here; the popover shows the whole text.
    static let inlineLineLimit = 3
}

/// User-readable text for the engine's typed `OperatorQuestionError`
/// (`{"code": "not_found" | "conflict" | "validation_failed", ...}`).
enum OperatorQuestionFailure {
    static func message(from value: Any?) -> String {
        guard let dict = value as? [String: Any], let code = dict["code"] as? String else {
            return "The engine refused the answer."
        }
        switch code {
        case "not_found":
            return "This question no longer exists."
        case "conflict":
            let state = (dict["state"] as? String) ?? "closed"
            var message = "This question can no longer be answered (it was \(state))."
            if let answer = dict["answer"] as? [String: Any] {
                if let yes = answer["value"] as? Bool {
                    message += " Recorded answer: \(yes ? "Yes" : "No")."
                } else if let choice = answer["value"] as? String {
                    message += " Recorded answer: \(choice)."
                }
            }
            return message
        case "validation_failed":
            let detail = (dict["message"] as? String) ?? "the answer does not fit the question"
            return "The engine rejected the answer: \(detail)"
        default:
            return "The engine refused the answer (\(code))."
        }
    }
}
