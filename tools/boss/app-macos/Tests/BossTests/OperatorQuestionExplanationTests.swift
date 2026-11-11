import XCTest
@testable import Boss

final class OperatorQuestionExplanationTests: XCTestCase {
    private let plain = """
    Context:
    Edits can be lost when the connection drops.
    Yes:
    Resend only actions that never left. Smaller change.
    No:
    Keep work blocked pending duplicate detection. Bigger change.
    Recommendation:
    I recommend Yes to avoid double actions.
    """

    func testPlainDecisionWithoutDetails() throws {
        let value = try XCTUnwrap(OperatorQuestionExplanation(plain))
        XCTAssertEqual(value.context, "Edits can be lost when the connection drops.")
        XCTAssertEqual(value.yes, "Resend only actions that never left. Smaller change.")
        XCTAssertEqual(value.no, "Keep work blocked pending duplicate detection. Bigger change.")
        XCTAssertEqual(value.recommendation, "I recommend Yes to avoid double actions.")
        XCTAssertNil(value.details)
    }

    func testTechnicalEvidenceIsPreservedIncludingHeadingsAndLongText() throws {
        let evidence = "EngineClient.swift\nhttps://example.com/evidence\nYes:\n" + String(repeating: "evidence\n", count: 500) + "end"
        let value = try XCTUnwrap(OperatorQuestionExplanation(plain + "\nDetails:\n" + evidence))
        XCTAssertEqual(value.details, evidence)
        XCTAssertFalse(value.context.contains("EngineClient"))
        XCTAssertFalse(try XCTUnwrap(value.no).contains("example.com"))
    }

    func testIncompleteOrLegacyTextFallsBackToVerbatimPresentation() {
        for text in [
            "", "Legacy explanation with EngineClient.swift and a link",
            "Context:\nOnly context", plain.replacingOccurrences(of: "Yes:\n", with: ""),
            plain.replacingOccurrences(of: "Yes:", with: "No:"),
            "Unlabeled evidence must not disappear\n" + plain,
            plain.replacingOccurrences(of: "I recommend Yes to avoid double actions.", with: ""),
        ] {
            XCTAssertNil(OperatorQuestionExplanation(text), text)
        }
    }

    func testIndentedLabelsAndWindowsNewlines() throws {
        let text = plain.replacingOccurrences(of: "\n", with: "\r\n  ")
        XCTAssertEqual(try XCTUnwrap(OperatorQuestionExplanation(text)).yes,
                       "Resend only actions that never left. Smaller change.")
    }

    func testMultipleChoiceShapeHasNoYesNoSections() throws {
        let choice = "Context:\nPick a rollout.\nRecommendation:\nGradual, to limit risk."
        let value = try XCTUnwrap(OperatorQuestionExplanation(choice))
        XCTAssertEqual(value.context, "Pick a rollout.")
        XCTAssertNil(value.yes)
        XCTAssertNil(value.no)
        XCTAssertEqual(value.recommendation, "Gradual, to limit risk.")
        XCTAssertNil(value.details)
        let withDetails = try XCTUnwrap(OperatorQuestionExplanation(choice + "\nDetails:\nYes:\nverbatim"))
        XCTAssertEqual(withDetails.details, "Yes:\nverbatim")
        XCTAssertNil(OperatorQuestionExplanation("Context:\nPick.\nRecommendation:\n"))
        XCTAssertNil(OperatorQuestionExplanation("Context:\nPick.\nYes:\nA\nRecommendation:\nB"))
    }

    private let named = """
    Context:
    The change is 48 files against a limit of 30.
    Split:
    Land the first PR, then finish the rest.
    Bypass limit:
    Keep one PR with a one-time exception.
    Recommendation:
    I recommend Bypass limit because the change is one unit.
    """
    private let labels = AnswerOptionLabels(yes: "Split", no: "Bypass limit")

    func testSectionsAreNamedForTheEnginesOptionLabels() throws {
        let value = try XCTUnwrap(OperatorQuestionExplanation(named, optionLabels: labels))
        XCTAssertEqual(value.yes, "Land the first PR, then finish the rest.")
        XCTAssertEqual(value.no, "Keep one PR with a one-time exception.")
        XCTAssertEqual(value.yesLabel, "Split")
        XCTAssertEqual(value.noLabel, "Bypass limit")
        XCTAssertEqual(value.recommendation, "I recommend Bypass limit because the change is one unit.")
        // Without the engine's names, the named sections are not Yes/No sections.
        XCTAssertNil(try XCTUnwrap(OperatorQuestionExplanation(named)).yes)
        let withDetails = try XCTUnwrap(OperatorQuestionExplanation(named + "\nDetails:\nBypass limit:\nverbatim", optionLabels: labels))
        XCTAssertEqual(withDetails.details, "Bypass limit:\nverbatim")
    }

    func testAnExplanationWrittenBeforeOptionsWereNamedStillReadsOnALabelledQuestion() throws {
        let value = try XCTUnwrap(OperatorQuestionExplanation(plain, optionLabels: labels))
        XCTAssertEqual(value.yesLabel, "Yes")
        XCTAssertEqual(value.noLabel, "No")
        XCTAssertEqual(value.yes, "Resend only actions that never left. Smaller change.")
        XCTAssertEqual(try XCTUnwrap(OperatorQuestionExplanation(plain)).yesLabel, "Yes")
    }
}
