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
            if CommandLine.arguments.contains("--capture-tall-fixture") {
                try writeTallFixture()
            }
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
                    if CommandLine.arguments.contains("--capture-viewer-zoom") {
                        try captureZoomStates(captureWindow, path: path)
                        NSApp.terminate(nil)
                    } else if CommandLine.arguments.contains("--capture-viewer-resize") {
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

    private static func captureZoomStates(_ window: NSWindow, path: String) throws {
        func find(_ view: NSView?) -> AttachmentZoomScrollView? {
            guard let view else { return nil }
            return (view as? AttachmentZoomScrollView) ?? view.subviews.lazy.compactMap { find($0) }.first
        }
        guard let viewer = find(window.contentView) else {
            throw NSError(domain: "AttachmentViewerCapture", code: 2,
                          userInfo: [NSLocalizedDescriptionKey: "Zoom viewer was not mounted"])
        }
        viewer.apply(.width)
        try BossWindowCapture.captureWindow(window, to: path + ".width.png")
        viewer.apply(.actual)
        viewer.apply(.in)
        viewer.apply(.in)
        viewer.apply(.in)
        viewer.apply(.in)
        try BossWindowCapture.captureWindow(window, to: path + ".zoom.png")
    }

    private static func writeTallFixture() throws {
        guard let path = CommandLine.arguments.first(where: { $0.hasPrefix("--capture-attachment-image=") })
            .map({ String($0.dropFirst("--capture-attachment-image=".count)) }) else { return }
        guard let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 390, pixelsHigh: 6245,
                                           bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true,
                                           isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0),
              let context = NSGraphicsContext(bitmapImageRep: bitmap) else { return }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = context
        NSColor.white.setFill()
        NSRect(x: 0, y: 0, width: 390, height: 6245).fill()
        for index in 0..<25 {
            let y = CGFloat(6245 - (index + 1) * 245)
            NSColor(calibratedRed: 0.9, green: 0.94, blue: 0.98, alpha: 1).setFill()
            NSRect(x: 12, y: y, width: 366, height: 230).fill()
            let text = "Fidelity notes · Section \(index + 1)\n\nMobile capture at 390px\nReview the layout and spacing.\nEach section should be readable."
            (text as NSString).draw(in: NSRect(x: 24, y: y + 20, width: 342, height: 190),
                                   withAttributes: [.font: NSFont.systemFont(ofSize: 17),
                                                    .foregroundColor: NSColor.black])
        }
        NSGraphicsContext.restoreGraphicsState()
        guard let png = bitmap.representation(using: .png, properties: [:]) else { return }
        try png.write(to: URL(fileURLWithPath: path))
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
