import AppKit
import SwiftUI
import UpdateCore

/// Quiet captures of review, merging, working and landed detail. All task
/// facts are fixtures; these images are not evidence of a GitHub merge.
@MainActor
enum BossNavigatorReviewCapture {
    private static var window: NSWindow?

    static func start(path: String, state: String, updateModel: UpdateModel) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let kind = String(state.dropFirst("navigator-review-".count))
        let product = WorkProduct(id: "review-capture", name: "Navigator review", slug: "navigator-review",
                                  description: "", repoRemoteURL: nil, status: "active",
                                  createdAt: "", updatedAt: "")
        var root = WorkTask(
            id: "root", productID: product.id, projectID: nil, kind: "task",
            name: "Keep keyboard focus while work progresses", description: "Preserve selection and the current revision ask.",
            status: kind == "landed" ? "done" : "in_review", priority: "medium", ordinal: 0,
            prURL: "https://github.com/spinyfin/mono/pull/3629", deletedAt: nil,
            createdAt: "2026-11-16T12:00:00Z", updatedAt: "2026-11-17T12:00:00Z"
        )
        root.reviewGuideLifecycle = "ready"
        root.reviewGuideReadableVersionId = "guide-current"
        root.reviewGuideSelectedComparisonId = "comparison-current"
        root.ciRequiredState = "success"
        if kind == "merging" { root.mergeQueueState = "queued" }
        let section = kind == "merging" ? "backlog"
            : kind == "landed" ? "landed"
            : kind == "working" ? "working"
            : kind == "ready" ? "ready"
            : "needs_attention"
        let label = kind == "merging" ? "Merging"
            : kind == "landed" ? "Merged"
            : kind == "working" ? "Fixing CI"
            : kind == "ready" ? "Review"
            : "Ready for review"
        var projection: [String: Any] = [
            "section": section, "sort_key": "0", "default_detail": "review",
            "reason": ["type": "wait", "wait": ["kind": "merging"]], "reason_label": label,
            "revisions": ["open": kind == "working" ? 1 : 0, "total": 2],
        ]
        if kind == "landed" {
            projection["merge_evidence"] = ["pr_url": root.prURL!, "observed_at": "2026-11-17T12:00:00Z"]
        }
        root.navigator = NavigatorProjection.parse(["version": 1, "root_task_id": root.id, "root": projection])
        model.products = [product]
        model.selectWorkProduct(product.id)
        model.isConnected = true
        model.applyWorkTree(product: product, projects: [], tasks: [root], chores: [], taskRuntimes: [],
                            dependencies: [], ideas: [], attentionItems: [], attentionGroups: [], attentionMembers: [])
        model.setNavigationMode(.work, refreshWorkOnEntry: false)
        model.selectWorkCard(root.id)
        model.setWorkViewMode(.navigator)
        model.navigator.tab = .review
        do {
            model.attachmentsByTaskID[root.id] = try BossReviewGuideAttachmentCapture.attachments(taskId: root.id)
        } catch {
            fputs("navigator review evidence fixture failed: \(error)\n", stderr)
            exit(1)
        }
        let stale = kind == "working"
        let version = ReviewGuideVersionContent(
            id: stale ? "guide-earlier" : "guide-current", seriesId: "series", comparisonId: stale ? "comparison-earlier" : "comparison-current",
            attemptId: "attempt", markdown: """
            ## What changed
            The Work navigator keeps the original task selected while revisions run, questions arrive and the pull request merges.

            ## Review steps
            1. Open a working revision and check the Current ask.
            2. Read the guide without leaving the agent activity behind.
            3. Answer the question and verify the selected task stays in place.

            ## Validation
            The guide explains the change. CI, review and merge readiness are shown separately.
            """, contentHash: "capture", promptVersion: "1", generatedAt: "2026-11-17T11:30:00Z"
        )
        model.navigatorReviewGuides.load(root: root, versionID: version.id) { _ in "capture" }
        model.navigatorReviewGuides.receive(versionID: version.id, content: version, requestID: "capture")
        model.reviewGuideFindingsBySeriesID["series"] = ReviewGuideFindings(statusText: "No open findings", addendumMarkdown: "")
        let width = CommandLine.arguments.first { $0.hasPrefix("--capture-review-width=") }
            .flatMap { Double($0.dropFirst("--capture-review-width=".count)) } ?? 1440
        let captureWindow = NSWindow(contentRect: NSRect(x: 0, y: 0, width: width, height: 900),
                                     styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
        captureWindow.title = "Boss [agent capture]"
        // Like the Attention fixture, host the navigator directly so the
        // capture exercises its detail without the outer NavigationSplitView.
        captureWindow.contentView = NSHostingView(rootView: WorkNavigatorView(model: model)
            .frame(width: width, height: 900)
            .environmentObject(model).environmentObject(updateModel))
        window = captureWindow
        captureWindow.contentView?.layoutSubtreeIfNeeded()
        DispatchQueue.main.asyncAfter(deadline: .now() + max(1, BossCaptureArgs.shared.captureAfter)) {
            captureWindow.contentView?.layoutSubtreeIfNeeded()
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                NSApp.terminate(nil)
            } catch {
                fputs("navigator review capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }
}
