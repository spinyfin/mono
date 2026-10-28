import AppKit
import SwiftUI

/// Quiet capture of the real screenshot viewer with supplied evidence.
/// Use --capture-reveal-state=attachment-viewer with --capture-attachments=2
/// and --capture-attachment-image=<png>. --capture-viewer-resize also captures
/// the same mounted view after resizing and collapsing its sidebar.
@MainActor
enum BossAttachmentViewerCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        do {
            let model = ChatViewModel(paths: BossEnginePaths.production())
            let taskID = "attachment-viewer-capture"
            let attachments = try BossReviewGuideAttachmentCapture.attachments(
                taskId: taskID, captions: ["Portrait screenshot — all four edges should be visible"]
            )
            model.selectedAttachmentIDByTaskID[taskID] = attachments.first?.id
            let captureWindow = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 1000, height: 680),
                styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false
            )
            captureWindow.title = "Screenshots [agent capture]"
            captureWindow.contentView = NSHostingView(rootView:
                AttachmentViewerView(ref: AttachmentViewerRef(taskId: taskID)).environmentObject(model))
            window = captureWindow
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) {
                model.applyEventForTest(.attachmentsList(taskId: taskID, attachments: attachments))
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + BossCaptureArgs.shared.captureAfter + 0.5) {
                do {
                    try BossWindowCapture.captureWindow(captureWindow, to: path)
                    if CommandLine.arguments.contains("--capture-viewer-resize") {
                        captureWindow.setContentSize(NSSize(width: 760, height: 480))
                        captureResized(captureWindow, path: path)
                    } else {
                        NSApp.terminate(nil)
                    }
                } catch {
                    fputs("attachment viewer capture failed: \(error)\n", stderr)
                    exit(1)
                }
            }
        } catch {
            fputs("attachment viewer fixture failed: \(error)\n", stderr)
            exit(1)
        }
    }

    private static func captureResized(_ window: NSWindow, path: String) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
            do {
                try BossWindowCapture.captureWindow(window, to: path + ".resized.png")
                guard let split = splitController(in: window.contentView), let sidebar = split.splitViewItems.first else {
                    throw NSError(domain: "AttachmentViewerCapture", code: 1,
                                  userInfo: [NSLocalizedDescriptionKey: "Viewer sidebar was not mounted"])
                }
                sidebar.isCollapsed = true
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                    do {
                        try BossWindowCapture.captureWindow(window, to: path + ".collapsed.png")
                        NSApp.terminate(nil)
                    } catch {
                        fputs("collapsed viewer capture failed: \(error)\n", stderr)
                        exit(1)
                    }
                }
            } catch {
                fputs("resized viewer capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }

    private static func splitController(in view: NSView?) -> NSSplitViewController? {
        guard let view else { return nil }
        if let split = view as? NSSplitView, let controller = split.delegate as? NSSplitViewController {
            return controller
        }
        return view.subviews.lazy.compactMap { splitController(in: $0) }.first
    }
}
