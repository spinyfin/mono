import AppKit
import SwiftUI

/// Quiet fixture for the shared primary/revision Review card presentation.
@MainActor
enum BossRevisionReviewCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let pr = "https://github.com/spinyfin/mono/pull/42"
        func card(id: String, kind: String) -> WorkTask {
            var task = WorkTask(
                id: id, productID: "review-fixture", projectID: nil, kind: kind,
                name: kind == "revision" ? "Address review findings" : "Improve board review state",
                description: "", status: "in_review", priority: "medium", ordinal: nil,
                prURL: pr, deletedAt: nil, createdAt: "2026-10-30T00:00:00Z", updatedAt: "2026-10-30T00:00:00Z"
            )
            task.shortID = kind == "revision" ? 42 : 41
            if kind == "revision" {
                task.parentTaskId = "primary"
                task.revisionSeq = 1
                task.revisionParentPrUrl = pr
            }
            task.reviewSection = .aiReview
            task.aiReviewing = true
            task.aiReviewState = "reviewing"
            task.aiReviewBadge = AIReviewBadgePresentation(label: "Reviewing (AI)", systemImage: "sparkles", tooltip: "AI review in progress")
            task.ciRequiredState = "in_progress"
            task.reviewRequiredState = "pending"
            return task
        }
        let primary = card(id: "primary", kind: "chore")
        let revision = card(id: "revision", kind: "revision")
        model.choresByProductID = ["review-fixture": [primary]]
        model.productLevelRevisionsByProductID = ["review-fixture": [revision]]
        let reviewers = ["Spock", "Data"].enumerated().map { index, name in
            WorkerLiveState(
                name: name, slotId: 25 + index, runId: "review-\(index)", model: "fixture", shellPid: 0,
                lastEventAt: nil, currentTool: nil, lastToolEndedAt: nil, activity: .working,
                liveStatus: nil, liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true,
                agentType: "review", reviewStage: "reviewers", executionKind: "pr_review",
                workItemId: primary.id, prURL: pr
            )
        }
        model.liveWorkerStates.update(states: reviewers)
        let view = VStack(alignment: .leading, spacing: 16) {
            Text("Review · AI Review").font(.title2)
            HStack(alignment: .top, spacing: 20) {
                ForEach([primary, revision]) { task in
                    WorkBoardSectionItemsView(
                        items: [task], column: .review, boardStyle: .productDefault,
                        model: model, liveStates: model.liveWorkerStates
                    )
                    .frame(width: 350)
                }
            }
        }
        .padding(24)
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 768, height: 500),
            styleMask: [.titled], backing: .buffered, defer: false
        )
        captureWindow.contentView = NSHostingView(rootView: view)
        window = captureWindow
        DispatchQueue.main.asyncAfter(deadline: .now() + BossCaptureArgs.shared.captureAfter) {
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                NSApp.terminate(nil)
            } catch {
                fputs("revision review capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }
}
