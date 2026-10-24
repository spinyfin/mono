import AppKit
import SwiftUI

/// Quiet, isolated fixture: --capture-reveal-state=operator-question --capture-to.
@MainActor
enum BossOperatorQuestionCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let question = OperatorQuestionPresentation(
            questionID: "capture-question",
            text: "Only resend actions that definitely never left the app?",
            explanation: """
            Context:
            The app sometimes loses edits or board moves when its connection drops. Resending every failed message could make an action happen twice because the app cannot tell which messages arrived.
            Yes:
            Resend only messages that never left; show others as “may not have gone through” for you to check. Smaller change; avoids double actions.
            No:
            Keep work blocked until duplicate detection is authorized, so all failed messages can be resent safely. Bigger change.
            Recommendation:
            I recommend Yes because a double action is worse than asking you to check.
            Details:
            NWConnection contentProcessed does not acknowledge remote delivery.
            https://developer.apple.com/videos/play/wwdc2018/715/
            """,
            askedAt: "2026-10-14T12:00:00Z", answerInFlight: false, errorMessage: nil,
            runSummary: "Connection recovery needs a delivery decision."
        )
        let choiceQuestion = OperatorQuestionPresentation(
            questionID: "capture-choice-question",
            text: "Which rollout should I use for the new scheduler?",
            explanation: """
            Context:
            The brief allows several ways to switch to the new scheduler, and they differ in how much risk you take on and how long the switch takes.
            Recommendation:
            I recommend “Behind a flag” because you can turn it off without a release.
            """,
            askedAt: "2026-10-14T12:00:00Z", answerInFlight: false, errorMessage: nil,
            runSummary: "The rollout strategy needs a decision.",
            choices: ["Gradual, one product at a time", "All at once", "Behind a flag"]
        )
        let view = HStack(alignment: .top, spacing: 32) {
            VStack(alignment: .leading) {
                Text("Board card").font(.headline)
                WorkBoardCardOperatorQuestion(slice: .init(presentation: question))
                Text("Board card — multiple choice").font(.headline).padding(.top, 16)
                WorkBoardCardOperatorQuestion(slice: .init(presentation: choiceQuestion))
            }.frame(width: 300)
            VStack(alignment: .leading) {
                Text("Why? — details collapsed initially").font(.headline)
                OperatorQuestionDetailPopover(question: question)
            }
            VStack(alignment: .leading) {
                Text("Why? — details expanded").font(.headline)
                OperatorQuestionDetailPopover(question: question, detailsExpanded: true)
            }
        }
        .padding(24)
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1120, height: 780),
            styleMask: [.titled], backing: .buffered, defer: false
        )
        captureWindow.title = "Boss [agent capture]"
        captureWindow.contentView = NSHostingView(rootView: view)
        window = captureWindow
        DispatchQueue.main.asyncAfter(deadline: .now() + BossCaptureArgs.shared.captureAfter) {
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                NSApp.terminate(nil)
            } catch {
                fputs("question capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }
}
