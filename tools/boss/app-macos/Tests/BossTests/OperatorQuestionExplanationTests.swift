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
        XCTAssertFalse(value.no.contains("example.com"))
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
}
