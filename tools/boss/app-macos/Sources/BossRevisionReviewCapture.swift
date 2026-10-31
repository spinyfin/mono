import AppKit
import SwiftUI

/// Quiet fixture for a revision's life around its PR's automated review.
/// `revision-ai-review` shows the revision held in Doing's Reviewing group
/// with its live review workers while the parent sits in Review.
/// `revision-folded` shows the same revision once the review is consolidated:
/// no card in any column, and an inline line on the parent's Review card.
@MainActor
enum BossRevisionReviewCapture {
    private static var window: NSWindow?

    static func start(path: String, folded: Bool) {
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let pr = "https://github.com/spinyfin/mono/pull/42"
        var parent = WorkTask(
            id: "primary", productID: "review-fixture", projectID: nil, kind: "chore",
            name: "Improve board review state", description: "", status: "in_review",
            priority: "medium", ordinal: nil, prURL: pr, deletedAt: nil,
            createdAt: "2026-10-30T00:00:00Z", updatedAt: "2026-10-30T00:00:00Z"
        )
        parent.shortID = 41
        parent.reviewSection = folded ? .ready : .inRevision
        parent.ciRequiredState = "success"
        parent.hasInProgressRevision = !folded
        if folded {
            parent.aiReviewState = "reviewed_all_clear"
            parent.aiReviewBadge = AIReviewBadgePresentation(
                label: "AI review: clean", systemImage: "checkmark.seal", tooltip: "AI review found no issues"
            )
        }
        var revision = WorkTask(
            id: "revision", productID: "review-fixture", projectID: nil, kind: "revision",
            name: "Address review findings", description: "", status: folded ? "in_review" : "active",
            priority: "medium", ordinal: nil, prURL: folded ? nil : pr, deletedAt: nil,
            createdAt: "2026-10-30T00:00:00Z", updatedAt: "2026-10-30T00:00:00Z"
        )
        revision.shortID = 42
        revision.parentTaskId = parent.id
        revision.revisionSeq = 1
        revision.revisionParentPrUrl = pr
        revision.aiReviewing = !folded
        revision.aiReviewStage = folded ? nil : "reviewers"
        model.choresByProductID = ["review-fixture": [parent]]
        model.productLevelRevisionsByProductID = ["review-fixture": [revision]]
        model.selectedWorkProductID = "review-fixture"
        model.includeChores = true
        let reviewers = folded ? [] : ["Spock", "Data"].enumerated().map { index, name in
            WorkerLiveState(
                name: name, slotId: 25 + index, runId: "review-\(index)", model: "fixture", shellPid: 0,
                lastEventAt: nil, currentTool: nil, lastToolEndedAt: nil, activity: .working,
                liveStatus: nil, liveStatusAt: nil, recoveryStatus: nil, tmuxHosted: true,
                agentType: "review", reviewStage: "reviewers", executionKind: "pr_review",
                workItemId: parent.id, prURL: pr
            )
        }
        model.liveWorkerStates.update(states: reviewers)

        // The Doing column holds only what `workItems(in:)` leaves in it: the
        // revision while it is under review, nothing once it has folded.
        let doing = model.workItems(in: .doing)
        let review = model.workItems(in: .review)
        func column(_ title: String, _ subtitle: String?, _ key: WorkBoardColumnKey, _ items: [WorkTask]) -> some View {
            VStack(alignment: .leading, spacing: 8) {
                Text(title).font(.title3).bold()
                if let subtitle { Text(subtitle).font(.caption).foregroundStyle(.secondary) }
                if items.isEmpty {
                    Text("No cards").font(.caption).foregroundStyle(.secondary)
                } else {
                    WorkBoardSectionItemsView(
                        items: items, column: key, boardStyle: .productDefault,
                        model: model, liveStates: model.liveWorkerStates
                    )
                }
            }
            .frame(width: 380, alignment: .topLeading)
        }
        let view = HStack(alignment: .top, spacing: 24) {
            column("Doing", doing.isEmpty ? nil : "Reviewing", .doing, doing)
            column("Review", folded ? "Ready" : "In Revision", .review, review)
        }
        .padding(24)
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 860, height: 420),
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
