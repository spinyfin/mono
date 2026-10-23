import AppKit
import SwiftUI

/// Quiet, isolated fixture for the screenshot viewer's sidebar rows:
/// `--capture-reveal-state=attachment-sidebar --capture-to <png>
/// --capture-attachments=<n> --capture-attachment-image=<png>`.
///
/// Renders the real `AttachmentRow` at the viewer's sidebar widths with a mix
/// of short, wrapping and pathological captions, so a regression where the
/// thumbnail covers the caption is visible.
@MainActor
enum BossAttachmentSidebarCapture {
    private static var window: NSWindow?

    private static let captions = [
        "Short caption",
        "Evidence viewer sidebar after the fix: thumbnails sit in a fixed slot and the caption wraps beside them in full",
        "Wide screenshot — after",
        "A pathologically long caption that keeps going well past the wrapping limit " + String(repeating: "and on and on ", count: 40),
    ]

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let attachments: [AttachmentVM]
        do {
            attachments = try BossReviewGuideAttachmentCapture.attachments(taskId: "sidebar-capture", captions: captions)
        } catch {
            fputs("attachment sidebar fixture failed: \(error)\n", stderr)
            exit(1)
        }
        let view = HStack(alignment: .top, spacing: 24) {
            ForEach([CGFloat(240), 280, 340], id: \.self) { width in
                VStack(alignment: .leading, spacing: 6) {
                    Text("Sidebar: \(Int(width)) pt").font(.headline)
                    List {
                        ForEach(attachments) { AttachmentRow(attachment: $0) }
                    }
                    .frame(width: width, height: 520)
                }
            }
        }
        .padding(20)
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1000, height: 600),
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
                fputs("attachment sidebar capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }
}
