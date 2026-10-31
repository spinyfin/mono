import AppKit
import Foundation
import SwiftUI
import UpdateCore

/// Repeatable real-view reveal validation in the existing quiet capture app.
/// Pass --capture-reveal-state=done (or another lifecycle state) alongside
/// --capture-to. Fixtures use the isolated socket and defaults only.
/// Add --capture-review-footer=with-revision (or without-revision) with
/// --capture-reveal-state=in_review to capture a single default-width Review card.
///
/// Scroll-stability knobs, for checking that a revealed card stays in view
/// while the board changes under it (all optional):
/// - `--capture-reveal-rows=<n>` cards in the column (default 30) and
///   `--capture-reveal-target=<index>` which one to reveal (default: last).
/// - `--capture-reveal-variable-heights` gives every third card a wrapping
///   name so row heights vary.
/// - `--capture-reveal-settle=<seconds>` waits that long after the reveal is
///   confirmed before capturing (default 0.3).
/// - `--capture-reveal-refreshes=<n>` re-applies the work tree n times after
///   the reveal, 0.4s apart, like engine refreshes. Combine with
///   `--capture-reveal-refresh-touch` (bump activity on a few other cards),
///   `--capture-reveal-refresh-touch-target` (bump the revealed card, which
///   moves it to the top of Review ▸ Pending) or
///   `--capture-reveal-refresh-touch-index=<index>` (bump one chosen card).
/// - `--capture-reveal-collapsed` starts with collapsible sections closed;
///   fails if the target does not belong to a collapsible section.
/// - `--capture-dump-scroll` prints every scroll view's visible rect and
///   document size every half second and at capture time.
@MainActor
enum BossRevealCapture {
    private static var window: NSWindow?
    static private(set) var viewportTargetID: String?
    static weak var targetViewport: NSView?
    static var state: String? {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return nil }
        return CommandLine.arguments.first { $0.hasPrefix("--capture-reveal-state=") }
            .map { String($0.dropFirst("--capture-reveal-state=".count)) }
    }

    /// Position of a fixture card in `rows`. Fixture variants trim the row
    /// list, so a card is found by id rather than by its original index.
    nonisolated static func index(ofCard id: String, in rows: [WorkTask]) -> Int? {
        rows.firstIndex { $0.id == id }
    }

    static func start(updateModel: UpdateModel) {
        guard let state, let path = BossCaptureArgs.shared.captureTo else { return }
        if state == "revision-ai-review" || state == "revision-folded" {
            BossRevisionReviewCapture.start(path: path, folded: state == "revision-folded")
            return
        }
        if state == "revision-brief" {
            BossRevisionBriefCapture.start(path: path)
            return
        }
        if state == "agents-toolbar" {
            BossAgentsToolbarCapture.start(path: path)
            return
        }
        if state == "board-loading" {
            BossBoardLoadingCapture.start(path: path, updateModel: updateModel)
            return
        }
        if state == "exited-worker" {
            BossExitedWorkerCapture.start(path: path)
            return
        }
        if state == "badge-width" {
            BossBadgeWidthCapture.start(path: path)
            return
        }
        if state == "operator-question" {
            BossOperatorQuestionCapture.start(path: path)
            return
        }
        if state == "queued-order" {
            BossQueuedOrderCapture.start(path: path, updateModel: updateModel)
            return
        }
        if state == "attachment-viewer" {
            BossAttachmentViewerCapture.start(path: path)
            return
        }
        if state == "attachment-sidebar" {
            BossAttachmentSidebarCapture.start(path: path)
            return
        }
        if state == "review-guide" {
            BossReviewGuideCapture.start(path: path)
            return
        }
        if state == "agent-pane-activity" {
            BossAgentPaneActivityCapture.start(path: path)
            return
        }
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let now = ISO8601DateFormatter().string(from: Date())
        let product = WorkProduct(
            id: "reveal-fixture", name: "Reveal verification", slug: "reveal-fixture",
            description: "", repoRemoteURL: nil, status: "active", createdAt: now, updatedAt: now
        )
        model.products = [product]
        model.selectWorkProduct(product.id)
        model.setNavigationMode(.work)
        let footerMode = CommandLine.arguments.first { $0.hasPrefix("--capture-review-footer=") }
            .map { String($0.dropFirst("--capture-review-footer=".count)) }
        if let footerMode {
            guard state == "in_review", ["with-revision", "without-revision"].contains(footerMode) else {
                fputs("review footer capture requires in_review and with-revision or without-revision\n", stderr)
                exit(1)
            }
        }
        let rowCount = state == "doing-sections" ? 3 : CommandLine.arguments.first { $0.hasPrefix("--capture-reveal-rows=") }
            .flatMap { Int(String($0.dropFirst("--capture-reveal-rows=".count))) } ?? 30
        let variableHeights = CommandLine.arguments.contains("--capture-reveal-variable-heights")
        let targetIndex = state == "doing-sections" ? 2 : (footerMode == nil
            ? (CommandLine.arguments.first { $0.hasPrefix("--capture-reveal-target=") }
                .flatMap { Int(String($0.dropFirst("--capture-reveal-target=".count))) } ?? rowCount - 1)
            : 0)
        let targetID = "reveal-row-\(targetIndex)"
        var rows = (0..<max(rowCount, targetIndex + 1)).map { index in
            WorkTask(
                id: "reveal-row-\(index)", productID: product.id, projectID: nil, kind: "chore",
                name: index == targetIndex ? "REVEAL TARGET — \(state)"
                    : variableHeights && index % 3 == 0
                        ? "Other card \(index) with a much longer name that wraps onto several lines so that card heights in this column vary a lot between rows"
                        : "Other card \(index)",
                description: "Reveal viewport fixture", status: state == "missing" ? "todo" : state,
                priority: "medium", ordinal: index,
                prURL: index == targetIndex ? "https://github.com/spinyfin/mono/pull/3065" : nil,
                deletedAt: nil, createdAt: now, updatedAt: now
            )
        }
        if let footerMode {
            rows[0].name = "Review card footer"
            rows[0].shortID = 1234
            rows[0].ciRequiredState = "in_progress"
            rows[0].prMergeableState = "conflicting"
            rows[0].hasInProgressRevision = footerMode == "with-revision"
        }
        if state == "missing" { rows.removeAll { $0.id == targetID } }
        var runtimes: [WorkTaskRuntime] = []
        if state == "queued" {
            rows = Array(rows.suffix(2))
            rows[0].status = "active"
            rows[0].name = "Earlier review revision"
            rows[0].shortID = 41
            rows[1].status = "todo"
            rows[1].autostart = true
            rows[1].name = "Apply the next review findings"
            rows[1].shortID = 42
            runtimes = [WorkTaskRuntime(
                workItemID: rows[1].id, executionStatus: "waiting_dependency", runStatus: nil,
                executionID: "capture-pending", dispatchRetryAt: nil,
                dispatchWaitReason: "waiting_dependency", dispatchWaitSince: nil,
                dispatchWaitBlocker: DispatchWaitBlocker(
                    workItemID: rows[0].id, productID: product.id, shortID: rows[0].shortID
                )
            )]
        }
        if state == "doing-sections" {
            for index in rows.indices {
                rows[index].status = "active"
                rows[index].name = ["Implement the change", "Review the pull request", "Waiting for dispatch"][index]
                rows[index].prURL = nil
            }
            model.applyEventForTest(.workerLiveStatesList(
                states: [], doingSections: [rows[0].id: .coding, rows[1].id: .reviewing]
            ))
        }
        model.applyEventForTest(.workTree(
            product: product, projects: [], tasks: [], chores: rows,
            taskRuntimes: runtimes, dependencies: [], ideas: []
        ))
        viewportTargetID = targetID
        targetViewport = nil
        let collapsed = CommandLine.arguments.contains("--capture-reveal-collapsed")
        let sections = WorkBoardColumnKey.allCases.flatMap { model.workSections(in: $0) }
        if collapsed && !sections.contains(where: { $0.isCollapsible && $0.items.contains { $0.id == targetID } }) {
            fputs("collapsed reveal fixture requires a target in a collapsible section\n", stderr)
            exit(1)
        }
        for column in WorkBoardColumnKey.allCases {
            for section in model.workSections(in: column) where section.isCollapsible {
                BossDefaults.store.set(
                    state == "doing-sections" ? false : (collapsed ? section.defaultExpanded : false),
                    forKey: WorkBoardSectionCollapse.storageKey(sectionID: section.id)
                )
            }
        }
        // Command-line WindowGroup startup can produce no window on macOS.
        // Host the real ContentView explicitly for this opt-in capture only.
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1060, height: 680),
            styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false
        )
        captureWindow.title = "Boss [agent capture]"
        captureWindow.contentView = NSHostingView(rootView: ContentView()
            .environmentObject(model).environmentObject(updateModel))
        window = captureWindow
        captureWindow.contentView?.layoutSubtreeIfNeeded()
        // Let the real board mount, including the optional collapsed target section.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
            model.revealWorkCard(targetID, productID: product.id) { result in
                switch result {
                case .success:
                    guard model.revealHighlightID == targetID else {
                        fputs("reveal capture highlighted the wrong card\n", stderr)
                        exit(1)
                    }
                    print("revealed \(targetID) (\(state)); exact card visible and highlighted")
                    let settle = CommandLine.arguments.first { $0.hasPrefix("--capture-reveal-settle=") }
                        .flatMap { TimeInterval(String($0.dropFirst("--capture-reveal-settle=".count))) } ?? 0.3
                    let refreshes = CommandLine.arguments.first { $0.hasPrefix("--capture-reveal-refreshes=") }
                        .flatMap { Int(String($0.dropFirst("--capture-reveal-refreshes=".count))) } ?? 0
                    let touch = CommandLine.arguments.contains("--capture-reveal-refresh-touch")
                    let touchTarget = CommandLine.arguments.contains("--capture-reveal-refresh-touch-target")
                    for i in 0..<refreshes {
                        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4 + Double(i) * 0.4) {
                            var refreshed = rows
                            let stamp = ISO8601DateFormatter().string(from: Date().addingTimeInterval(Double(i + 1)))
                            if touch && (!touchTarget || i > 0) {
                                // Mimic activity on a few other PRs so Pending's
                                // recency order changes around the target.
                                for j in stride(from: i, to: refreshed.count, by: 7) where refreshed[j].id != targetID {
                                    refreshed[j].updatedAt = stamp
                                }
                            }
                            if let touchIndex = CommandLine.arguments.first(where: { $0.hasPrefix("--capture-reveal-refresh-touch-index=") })
                                .flatMap({ Int(String($0.dropFirst("--capture-reveal-refresh-touch-index=".count))) }),
                               i == 0, refreshed.indices.contains(touchIndex) {
                                // Activity on one chosen card moves it to the top of Pending.
                                refreshed[touchIndex].updatedAt = stamp
                            }
                            if touchTarget, i == 0,
                               let index = Self.index(ofCard: targetID, in: refreshed) {
                                // Activity on the revealed card itself moves it to
                                // the top of Pending.
                                refreshed[index].updatedAt = stamp
                            }
                            rows = refreshed
                            print("applying simulated work-tree refresh \(i + 1) touch=\(touch)")
                            model.applyEventForTest(.workTree(
                                product: product, projects: [], tasks: [], chores: refreshed,
                                taskRuntimes: runtimes, dependencies: [], ideas: []
                            ))
                        }
                    }
                    // Wait for every refresh plus layout, even with the default settle argument.
                    let captureDelay = max(settle, Double(refreshes) * 0.4 + 0.5)
                    let t0 = Date()
                    @MainActor func tick() {
                        BossWindowCapture.dumpScrollOffsets(
                            in: captureWindow.contentView,
                            label: String(format: "t+%.1fs", Date().timeIntervalSince(t0)))
                        if Date().timeIntervalSince(t0) < captureDelay - 0.4 {
                            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { tick() }
                        }
                    }
                    tick()
                    DispatchQueue.main.asyncAfter(deadline: .now() + captureDelay) {
                        captureWindow.contentView?.layoutSubtreeIfNeeded()
                        guard let probe = targetViewport,
                              RevealCardViewport.isVisible(view: probe, board: model.revealBoardViewport) else {
                            fputs("revealed card left the viewport after refreshes: \(targetID)\n", stderr)
                            exit(1)
                        }
                        print("verified \(targetID) remains visible after \(refreshes) refreshes")
                        do {
                            try BossWindowCapture.captureWindow(captureWindow, to: path)
                            NSApp.terminate(nil)
                        } catch {
                            fputs("fixture capture failed: \(error)\n", stderr)
                            exit(1)
                        }
                    }
                case .failure(.internalFailure(let reason)):
                    fputs("reveal failed: \(reason)\n", stderr)
                    exit(1)
                }
            }
        }
    }
}
