import AppKit
import SwiftUI

/// Captures the task-description viewer with the exact wrapped review brief.
@MainActor
enum BossRevisionBriefCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let model = ChatViewModel(paths: BossEnginePaths.production())
        model.openLoadedMarkdown(title: "PR review revision brief", markdown: RevisionBriefCaptureFixture.markdown)
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
}
