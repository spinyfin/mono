import AppKit
import SwiftUI

/// Quiet real-document fixture for list rhythm:
/// `--capture-reveal-state=markdown-list-rhythm --capture-to <png>`.
///
/// Hosts the production `MarkdownDocumentChrome` at the real
/// `async-markdown-viewer` scene width with a "Goals" list whose bullets each
/// wrap to two or three lines — the shape that shows whether wrapped lines sit
/// closer together than consecutive bullets. Captured through the same
/// never-presented window path as the other fixtures.
@MainActor
enum BossMarkdownListCapture {
    private static var window: NSWindow?

    static let source = """
        # Markdown list rhythm

        Body prose above the list, long enough to wrap onto a second line so the leading inside a paragraph can be compared with the leading inside a bullet.

        ## Goals

        - Line spacing within a list item matches body paragraph line spacing, so a bullet that wraps reads as one item rather than as lines drifting apart.
        - The gap between consecutive list items is at least the in-item line gap, and a little more, so the list reads as separate items instead of one block of text.
        - Holds for single-line items too.
        - Holds for items with `inline code` and [links](https://example.com) whose decorations must not change the rhythm of the lines around them.
        - Nested lists share the same item gap as their parent:
          - a nested bullet that also wraps onto a second line keeps the same leading and the same separation from its sibling
          - as this one does
        """

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 760, height: 760),
            styleMask: [.titled], backing: .buffered, defer: false
        )
        captureWindow.title = "Boss [agent capture]"
        captureWindow.contentView = NSHostingView(
            rootView: MarkdownDocumentChrome(
                title: "Design doc", source: source, commentsEnabled: false))
        window = captureWindow
        captureWindow.contentView?.layoutSubtreeIfNeeded()
        DispatchQueue.main.asyncAfter(deadline: .now() + BossCaptureArgs.shared.captureAfter) {
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                NSApp.terminate(nil)
            } catch {
                fputs("markdown list capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }
}
