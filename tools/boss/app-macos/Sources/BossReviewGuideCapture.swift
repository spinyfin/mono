import AppKit
import SwiftUI

/// Quiet, isolated fixture for the review-guide viewer's findings panel:
/// `--capture-reveal-state=review-guide --capture-to <png>`.
///
/// Options:
/// - `--capture-findings=<n>` — how many findings the addendum lists
///   (default 2). `0` renders the shape the engine sends while a guide
///   update is pending with no tracked findings: a status line and an
///   empty addendum.
/// - `--capture-findings-collapsed` — start with the addendum disclosure
///   collapsed instead of expanded.
///
/// Hosts the same header + document composition `AsyncMarkdownViewerView`
/// mounts for a loaded guide, with a disconnected `ChatViewModel` seeded
/// the way `applyReviewGuideContent` and the findings-summary reply would
/// seed it.
@MainActor
enum BossReviewGuideCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let arguments = CommandLine.arguments
        let count = arguments.first { $0.hasPrefix("--capture-findings=") }
            .flatMap { Int($0.dropFirst("--capture-findings=".count)) } ?? 2
        let collapsed = arguments.contains("--capture-findings-collapsed")

        let model = ChatViewModel(paths: BossEnginePaths.production())
        let seriesID = "prgs_capture"
        let versionID = "prgv_capture"
        var task = WorkTask(
            id: "review-guide-capture", productID: "fixture", projectID: nil, kind: "task",
            name: "Findings panel fixture", description: "", status: "in_review",
            priority: "medium", ordinal: 0,
            prURL: "https://github.com/spinyfin/mono/pull/3065", deletedAt: nil,
            createdAt: "2026-10-20T12:00:00Z", updatedAt: "2026-10-20T12:00:00Z"
        )
        task.reviewGuideLifecycle = "ready"
        task.reviewGuideReadableVersionId = versionID
        model.taskIndexByID = [task.id: task]
        model.reviewGuideFindingsBySeriesID[seriesID] = findings(count: count)
        // The guide on screen is the readable version, so the currentness
        // banner stays quiet, as it does for a freshly opened guide.
        model.pendingReviewGuideVersionId = versionID
        model.pendingReviewGuideRootTaskId = task.id

        let vm = model.asyncMarkdownViewerVM
        vm.reviewGuideRootTaskId = task.id
        vm.reviewGuideGeneratedAt = "2026-10-20T12:00:00Z"
        vm.reviewGuideComparisonId = "prgc_capture"
        vm.state = .loaded(
            title: "Review guide: \(task.name)",
            markdown: guideMarkdown,
            artifact: .reviewGuide(seriesID: seriesID, versionID: versionID)
        )

        let view = VStack(spacing: 0) {
            ReviewGuideViewerHeader(
                chatModel: model,
                rootTaskId: task.id,
                generatedAt: vm.reviewGuideGeneratedAt,
                findingsInitiallyExpanded: !collapsed
            )
            MarkdownViewerView(
                title: "Review guide: \(task.name)",
                source: guideMarkdown,
                artifact: .reviewGuide(seriesID: seriesID, versionID: versionID)
            )
        }
        .environmentObject(model)
        .environmentObject(vm)

        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 900, height: 720),
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
                fputs("review guide capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }

    /// Mirrors the engine's `FindingsText::finish` wording: a status line
    /// plus one addendum bullet per finding, cycling through the open /
    /// in-progress / fixed markers.
    private static func findings(count: Int) -> ReviewGuideFindings {
        guard count > 0 else {
            return ReviewGuideFindings(
                statusText: "Updating after revision 1234",
                addendumMarkdown: ""
            )
        }
        var lines: [String] = []
        var fixed = 0
        var inProgress = 0
        var open = 0
        for index in 0..<count {
            let severity = ["high", "medium", "low"][index % 3]
            let title = "[\(severity)] \(findingTitles[index % findingTitles.count])"
            switch index % 3 {
            case 1:
                inProgress += 1
                lines.append("- ◷ \(title) — ID 1\(index) — in progress")
            case 2:
                fixed += 1
                lines.append("- ✓ ~~\(title)~~ — ID 1\(index)")
            default:
                open += 1
                lines.append("- \(title)")
            }
        }
        return ReviewGuideFindings(
            statusText: "\(count) finding\(count == 1 ? "" : "s"): \(fixed) fixed on PR, \(inProgress) in progress, \(open) open",
            addendumMarkdown: lines.joined(separator: "\n")
        )
    }

    private static let findingTitles = [
        "Unchecked index into the findings array",
        "Retry loop never backs off on a 429",
        "Stale snapshot read after the lease is released",
        "Missing test for the collapsed disclosure state",
        "Error banner uses a modal instead of an inline note",
    ]

    private static let guideMarkdown = """
    # What this PR changes

    The review guide viewer's findings panel sizes its addendum list to its \
    content, up to a height cap.

    ## Why

    Short lists fit their content, and longer lists scroll once they pass \
    the cap.

    ## How to review

    - Open a guide with two findings and confirm the body starts right under \
    the addendum.
    - Collapse and re-expand the addendum; the panel should resize both ways.
    - Open a guide with many findings and confirm the list scrolls past the cap.

    ## Risk

    Layout only. No engine or protocol change.
    """
}
