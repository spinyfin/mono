import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// Renders the card's inline question block through a detached
/// `NSHostingView` (no `NSWindow` — that path segfaults under the bazel
/// XCTest host; see `BossCaptureTests`). Pins the two behaviours the data
/// layer cannot: the question truncates at three lines, and the Yes/No
/// buttons are visibly disabled while an answer is in flight.
@MainActor
final class OperatorQuestionCardRenderTests: XCTestCase {
    private let width: CGFloat = 280

    func testQuestionTruncatesAtThreeLines() throws {
        let oneLine = fittingHeight(of: presentation(text: "Approve the bypass?"))
        let sixLines = fittingHeight(of: presentation(text: words(count: 60)))
        let fortyLines = fittingHeight(of: presentation(text: words(count: 400)))
        guard oneLine > 0 else {
            throw XCTSkip("host did not lay the view out; offscreen SwiftUI sizing unavailable")
        }

        XCTAssertGreaterThan(sixLines, oneLine, "a long question must grow past a one-line question")
        XCTAssertEqual(
            sixLines, fortyLines, accuracy: 0.5,
            "past three lines the inline text is clamped, so a 500-character question is no taller than a six-line one"
        )
    }

    func testThreeLineLimitIsTheDocumentedInlineCap() {
        XCTAssertEqual(OperatorQuestionPresentation.inlineLineLimit, 3)
    }

    func testAnswerButtonsRenderDisabledWhileAnAnswerIsInFlight() throws {
        let idle = try render(presentation(answerInFlight: false))
        let inFlight = try render(presentation(answerInFlight: true))
        guard !isUniformlyBlank(idle) else {
            throw XCTSkip("render came back uniformly blank; host does not support offscreen SwiftUI rendering")
        }

        XCTAssertNotEqual(
            idle.representation(using: .png, properties: [:]),
            inFlight.representation(using: .png, properties: [:]),
            "disabled Yes/No buttons must draw differently from enabled ones"
        )
    }

    func testRefusalTextIsRenderedUnderTheButtons() throws {
        let plain = fittingHeight(of: presentation())
        let refused = fittingHeight(of: presentation(error: "This question no longer exists."))
        guard plain > 0 else {
            throw XCTSkip("host did not lay the view out; offscreen SwiftUI sizing unavailable")
        }
        XCTAssertGreaterThan(refused, plain)
    }

    // MARK: - Helpers

    private func presentation(
        text: String = "Approve bypass of the 30 max file limit (48 needed)?",
        answerInFlight: Bool = false,
        error: String? = nil
    ) -> OperatorQuestionPresentation {
        OperatorQuestionPresentation(
            questionID: "oq_1",
            text: text,
            explanation: "Sweeps 48 files.",
            askedAt: "1790000000",
            answerInFlight: answerInFlight,
            errorMessage: error
        )
    }

    private func words(count: Int) -> String {
        (0..<count).map { "word\($0)" }.joined(separator: " ")
    }

    private func block(_ presentation: OperatorQuestionPresentation) -> some View {
        WorkBoardCardOperatorQuestion(slice: .init(presentation: presentation))
            .padding(12)
            .frame(width: width)
            .background(Color(nsColor: .windowBackgroundColor))
    }

    private func fittingHeight(of presentation: OperatorQuestionPresentation) -> CGFloat {
        let host = NSHostingView(rootView: block(presentation))
        host.frame = NSRect(x: 0, y: 0, width: width, height: 10)
        host.layoutSubtreeIfNeeded()
        return host.fittingSize.height
    }

    private func render(_ presentation: OperatorQuestionPresentation) throws -> NSBitmapImageRep {
        let host = NSHostingView(rootView: block(presentation))
        let height = max(host.fittingSize.height, 60)
        host.appearance = NSAppearance(named: .aqua)
        host.frame = NSRect(x: 0, y: 0, width: width, height: height)
        host.layoutSubtreeIfNeeded()
        guard let rep = host.bitmapImageRepForCachingDisplay(in: host.bounds) else {
            throw XCTSkip("bitmapImageRepForCachingDisplay returned nil")
        }
        host.cacheDisplay(in: host.bounds, to: rep)
        return rep
    }

    private func isUniformlyBlank(_ rep: NSBitmapImageRep) -> Bool {
        guard let bytes = rep.bitmapData, rep.samplesPerPixel >= 3 else { return true }
        var first: [UInt8]?
        for y in stride(from: 0, to: rep.pixelsHigh, by: 4) {
            for x in stride(from: 0, to: rep.pixelsWide, by: 4) {
                let offset = y * rep.bytesPerRow + x * rep.samplesPerPixel
                let pixel = [bytes[offset], bytes[offset + 1], bytes[offset + 2]]
                if let seen = first {
                    if pixel != seen { return false }
                } else {
                    first = pixel
                }
            }
        }
        return true
    }
}
