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
/// - `--capture-dump-scroll` prints every scroll view's visible rect and
///   document size every half second and at capture time.
@MainActor
enum BossRevealCapture {
    private static var window: NSWindow?
    static var state: String? {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return nil }
        return CommandLine.arguments.first { $0.hasPrefix("--capture-reveal-state=") }
            .map { String($0.dropFirst("--capture-reveal-state=".count)) }
    }

    static func start(updateModel: UpdateModel) {
        guard let state, let path = BossCaptureArgs.shared.captureTo else { return }
        if state == "badge-width" {
            BossBadgeWidthCapture.start(path: path)
            return
        }
        if state == "operator-question" {
            BossOperatorQuestionCapture.start(path: path)
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
        let rowCount = CommandLine.arguments.first { $0.hasPrefix("--capture-reveal-rows=") }
            .flatMap { Int(String($0.dropFirst("--capture-reveal-rows=".count))) } ?? 30
        let variableHeights = CommandLine.arguments.contains("--capture-reveal-variable-heights")
        let targetIndex = footerMode == nil
            ? (CommandLine.arguments.first { $0.hasPrefix("--capture-reveal-target=") }
                .flatMap { Int(String($0.dropFirst("--capture-reveal-target=".count))) } ?? rowCount - 1)
            : 0
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
        model.applyEventForTest(.workTree(
            product: product, projects: [], tasks: [], chores: rows,
            taskRuntimes: runtimes, dependencies: [], ideas: []
        ))
        for column in WorkBoardColumnKey.allCases {
            for section in model.workSections(in: column) where section.isCollapsible {
                BossDefaults.store.set(
                    section.defaultExpanded,
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
        // Let the real board mount with its target section collapsed first.
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
                            let stamp = ISO8601DateFormatter().string(from: Date())
                            if touch {
                                // Mimic activity on a few other PRs so Pending's
                                // recency order changes around the target.
                                for j in stride(from: i, to: refreshed.count, by: 7) where j != targetIndex {
                                    refreshed[j].updatedAt = stamp
                                }
                            }
                            if let touchIndex = CommandLine.arguments.first(where: { $0.hasPrefix("--capture-reveal-refresh-touch-index=") })
                                .flatMap({ Int(String($0.dropFirst("--capture-reveal-refresh-touch-index=".count))) }),
                               i == 0, refreshed.indices.contains(touchIndex) {
                                // Activity on one chosen card moves it to the top of Pending.
                                refreshed[touchIndex].updatedAt = stamp
                            }
                            if touchTarget, i == 0 {
                                // Activity on the revealed card itself moves it to
                                // the top of Pending.
                                refreshed[targetIndex].updatedAt = stamp
                            }
                            print("applying simulated work-tree refresh \(i + 1) touch=\(touch)")
                            model.applyEventForTest(.workTree(
                                product: product, projects: [], tasks: [], chores: refreshed,
                                taskRuntimes: runtimes, dependencies: [], ideas: []
                            ))
                        }
                    }
                    let t0 = Date()
                    @MainActor func tick() {
                        BossWindowCapture.dumpScrollOffsets(
                            in: captureWindow.contentView,
                            label: String(format: "t+%.1fs", Date().timeIntervalSince(t0)))
                        if Date().timeIntervalSince(t0) < settle - 0.4 {
                            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { tick() }
                        }
                    }
                    tick()
                    DispatchQueue.main.asyncAfter(deadline: .now() + settle) {
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
