import AppKit
import SwiftUI

/// Captures the task-description viewer with a review brief supplied at
/// capture time via `--capture-brief-markdown=<path>`. Isolated capture
/// only; `start` returns immediately otherwise. The engine-shaped sample
/// lives in the test fixture; pass those bytes as the file so screenshots
/// cannot drift from the assertions.
@MainActor
enum BossRevisionBriefCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        guard let markdownPath = BossCaptureArgs.shared.captureBriefMarkdown, !markdownPath.isEmpty else {
            fputs("revision brief capture requires --capture-brief-markdown=<path>\n", stderr)
            exit(1)
        }
        let markdown: String
        do {
            markdown = try loadMarkdown(from: markdownPath)
        } catch {
            fputs("revision brief capture failed to read \(markdownPath): \(error)\n", stderr)
            exit(1)
        }
        let model = ChatViewModel(paths: BossEnginePaths.production())
        model.openLoadedMarkdown(title: "PR review revision brief", markdown: markdown)
        let expanded = CommandLine.arguments.contains("--capture-brief-expanded")
        if expanded {
            model.asyncMarkdownViewerVM.collapsedByDefaultHeadings = []
        }
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1100, height: expanded ? 1400 : 600),
            styleMask: [.titled], backing: .buffered, defer: false
        )
        captureWindow.contentView = NSHostingView(rootView: AsyncMarkdownViewerView()
            .environmentObject(model).environmentObject(model.asyncMarkdownViewerVM))
        window = captureWindow
        DispatchQueue.main.asyncAfter(deadline: .now() + BossCaptureArgs.shared.captureAfter) {
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                NSApp.terminate(nil)
            } catch {
                fputs("revision brief capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }

    /// Isolated-capture helper: read the markdown file named by
    /// `--capture-brief-markdown`. Tests pin this against
    /// `RevisionBriefCaptureFixture.engineShaped`.
    nonisolated static func loadMarkdown(from path: String) throws -> String {
        try String(contentsOf: URL(fileURLWithPath: path), encoding: .utf8)
    }
}
