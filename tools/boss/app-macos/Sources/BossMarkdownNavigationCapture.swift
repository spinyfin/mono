import AppKit
import SwiftUI
import Vision

/// Quiet real-renderer click fixture. Captures the top of a long document,
/// clicks its first link through Textual's AppKit overlay, then captures the destination.
@MainActor
enum BossMarkdownNavigationCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let source = "[The concrete evaluation](#concrete-evaluation)\n\n"
            + (1...24).map { "## Background \($0)\n\n" + String(repeating: "Context for the evaluation. ", count: 30) }.joined(separator: "\n\n")
            + "\n\n## Concrete evaluation\n\nThe anchor link reached this heading.\n\n"
            + String(repeating: "Evaluation evidence follows.\n\n", count: 30)
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1000, height: 700),
            styleMask: [.titled], backing: .buffered, defer: false
        )
        captureWindow.contentView = NSHostingView(rootView: MarkdownDocumentChrome(
            title: "Markdown anchor navigation", source: source, commentsEnabled: false
        ))
        window = captureWindow
        captureWindow.contentView?.layoutSubtreeIfNeeded()
        DispatchQueue.main.asyncAfter(deadline: .now() + max(1, BossCaptureArgs.shared.captureAfter)) {
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path + ".before.png")
                guard let root = captureWindow.contentView,
                      let overlay = descendants(root).first(where: {
                          String(describing: type(of: $0)).contains("NSTextInteractionView")
                      }), let scroll = descendants(root).compactMap({ $0 as? NSScrollView }).first else {
                    preconditionFailure("Markdown link overlay or scroll view missing")
                }
                let before = scroll.contentView.bounds.origin.y
                let point = overlay.convert(NSPoint(x: 12, y: 12), to: nil)
                let event = NSEvent.mouseEvent(
                    with: .leftMouseDown, location: point, modifierFlags: [], timestamp: 0,
                    windowNumber: captureWindow.windowNumber, context: nil, eventNumber: 1,
                    clickCount: 1, pressure: 1
                )!
                overlay.mouseDown(with: event)
                overlay.mouseUp(with: event)
                DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                    precondition(scroll.contentView.bounds.origin.y > before + 1000, "Anchor click did not scroll")
                    do {
                        try BossWindowCapture.captureWindow(captureWindow, to: path)
                        let headingVisible = try visibleHeading(in: path)
                        precondition(headingVisible, "Destination heading is not visible")
                        fputs("Markdown link click scrolled from \(before) to \(scroll.contentView.bounds.origin.y)\n", stderr)
                        NSApp.terminate(nil)
                    } catch { fatalError("Markdown capture failed: \(error)") }
                }
            } catch { fatalError("Markdown capture failed: \(error)") }
        }
    }

    private static func descendants(_ view: NSView) -> [NSView] {
        [view] + view.subviews.flatMap(descendants)
    }

    private static func visibleHeading(in path: String) throws -> Bool {
        // Hidden capture windows do not expose an accessibility tree. Verify
        // the rendered pixels instead, including the heading's vertical position.
        let request = VNRecognizeTextRequest()
        request.recognitionLevel = .accurate
        request.recognitionLanguages = ["en-US"]
        try VNImageRequestHandler(url: URL(fileURLWithPath: path)).perform([request])
        return (request.results ?? []).contains {
            $0.topCandidates(1).first?.string == "Concrete evaluation" && $0.boundingBox.minY > 0.8
        }
    }
}
