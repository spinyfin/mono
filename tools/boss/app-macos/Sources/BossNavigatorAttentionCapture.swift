import AppKit
import SwiftUI

/// Quiet, isolated fixture: `--capture-reveal-state=navigator-attention --capture-to`.
///
/// Renders the shared question bodies, a terminal input wait, an attention
/// group, and Navigator chrome for a Working row that still has a question.
@MainActor
enum BossNavigatorAttentionCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let yesNo = OperatorQuestionPresentation(
            questionID: "oq_yes",
            text: "Approve the bypass?",
            explanation: """
            Context:
            Need a decision.
            Yes:
            Approve.
            No:
            Keep blocked.
            Recommendation:
            I recommend Yes because the change is already reviewed.
            """,
            askedAt: "2026-10-14T12:00:00Z", answerInFlight: false, errorMessage: nil
        )
        let split = OperatorQuestionPresentation(
            questionID: "oq_split",
            text: "Split this change or keep it in one PR?",
            explanation: """
            Context:
            The change is over the file-count limit.
            Split:
            Land a first PR, then finish the rest.
            Bypass limit:
            Keep everything in one PR.
            Recommendation:
            I recommend Split because the first PR is independently useful.
            """,
            askedAt: "2026-10-14T12:00:00Z", answerInFlight: false, errorMessage: nil,
            optionLabels: AnswerOptionLabels(yes: "Split", no: "Bypass limit")
        )
        let choice = OperatorQuestionPresentation(
            questionID: "oq_choice",
            text: "Which rollout should I use?",
            explanation: """
            Context:
            Pick a rollout.
            Recommendation:
            I recommend Behind a flag because you can turn it off.
            """,
            askedAt: "2026-10-14T12:00:00Z", answerInFlight: false, errorMessage: nil,
            choices: ["Gradual", "All at once", "Behind a flag"]
        )

        let model = makeModel()
        let group = AttentionGroup(
            id: "atg_prompt", productID: "product", shortID: nil, kind: "question",
            associationProjectID: nil, associationTaskID: "root",
            sourceKind: "task_transcript", sourceTaskID: "root", sourceRunID: nil,
            sourceDocPath: nil, sourceDocRepoRemoteURL: nil, sourceDocBranch: nil,
            groupingKey: "k_prompt", generation: 2, state: "open",
            producedArtifactKind: nil, producedArtifactRef: nil,
            createdAt: "2026-06-01T00:00:00Z", actionedAt: nil, dismissedAt: nil
        )
        let member = Attention(
            id: "atn_prompt", groupID: group.id, ordinal: 1, sourceAnchor: nil,
            answerState: "open", createdAt: "2026-06-01T00:00:00Z", answeredAt: nil,
            questionType: "prompt", promptText: "Which channel should the alert use?",
            choiceOptions: nil, answer: nil, proposedName: nil, proposedDescription: nil,
            proposedEffort: nil, proposedWorkKind: nil, rationale: nil,
            confidenceSource: "structured", score: 1
        )
        model.applyAttentionGroupSnapshot(group, members: [member])

        let view = VStack(alignment: .leading, spacing: 24) {
            Text("Navigator Attention").font(.title2)
            HStack(alignment: .top, spacing: 20) {
                labeled("Yes / No") { OperatorQuestionBody(question: yesNo) }
                labeled("Split / Bypass") { OperatorQuestionBody(question: split) }
                labeled("Multiple choice") { OperatorQuestionBody(question: choice) }
            }
            HStack(alignment: .top, spacing: 20) {
                labeled("Terminal input wait") {
                    NavigatorOpenInputBody(
                        activity: NavigatorActivity(executionId: "exec-wait", taskId: "root", runId: "run-wait"),
                        model: model
                    )
                }
                labeled("Attention group") {
                    OwnedAttentionGroupBody(group: group)
                        .environmentObject(model)
                }
            }
            labeled("Working row with a live question") {
                WorkNavigatorView(model: model)
                    .frame(width: 1000, height: 520)
            }
        }
        .padding(24)
        .frame(width: 1080)
        .background(Color(nsColor: .windowBackgroundColor))

        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1120, height: 1500),
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
                fputs("navigator attention capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }

    private static func labeled<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.headline)
            content()
                .padding(12)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(
                    RoundedRectangle(cornerRadius: 10)
                        .fill(Color(nsColor: .controlBackgroundColor))
                )
        }
    }

    private static func makeModel() -> ChatViewModel {
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let product = WorkProduct(
            id: "product", name: "Navigator", slug: "navigator", description: "",
            repoRemoteURL: nil, status: "active", createdAt: "", updatedAt: ""
        )
        var root = WorkTask(
            id: "root", productID: product.id, projectID: nil, kind: "task",
            name: "Improve navigation root", description: "A readable **brief** for root.",
            status: "active", priority: "medium", ordinal: nil,
            prURL: "https://github.com/spinyfin/mono/pull/3629", deletedAt: nil,
            createdAt: "", updatedAt: ""
        )
        root.operatorQuestion = OperatorQuestion(
            id: "oq_yes", text: "Approve the bypass?", answerType: .yesNo,
            explanation: "Need a decision.", askedAt: "1790000000", executionID: "exec-oq_yes"
        )
        root.navigator = NavigatorProjection.parse([
            "version": 1, "root_task_id": "root", "root": [
                "section": "working", "sort_key": "0", "default_detail": "activity",
                "reason": ["type": "active", "activity": "revising"], "reason_label": "Revising",
                "revisions": ["open": 1, "total": 1, "current_task_id": "revision"],
                "actions": [[
                    "type": "attention", "kind": "question", "attention_id": "oq_yes", "task_id": "root",
                ]],
            ] as [String: Any],
        ])
        model.applyWorkTree(
            product: product, projects: [], tasks: [root], chores: [], taskRuntimes: [],
            dependencies: [], ideas: [], attentionItems: [], attentionGroups: [], attentionMembers: []
        )
        model.setWorkViewMode(.navigator)
        model.selectWorkCard("root")
        model.navigator.tab = .attention
        return model
    }
}
