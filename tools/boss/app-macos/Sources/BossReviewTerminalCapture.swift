import AppKit
import SwiftUI

/// Quiet capture of the real terminal, including its local shell process.
@MainActor
enum BossReviewTerminalCapture {
    private static var window: NSWindow?

    static func start(path: String, state: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let vm = model.reviewTerminalVM
        vm.begin(workItemID: "terminal-capture", taskName: "Review terminal")
        vm.track(requestID: "terminal-capture-request")
        if state == "review-terminal-error" {
            vm.fail("Checkout failed: requested PR revision is unavailable. Close and reopen to retry.")
        } else if state == "review-terminal-ready" {
            vm.ready(ReviewTerminalContent(
                workItemID: "terminal-capture", workspacePath: NSTemporaryDirectory(), leaseID: nil,
                taskName: "Review terminal"
            ))
        } else {
            vm.noteProgress("Fetching PR branch")
        }
        if state == "review-terminal-transition" {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
                vm.ready(ReviewTerminalContent(
                    workItemID: "terminal-capture", workspacePath: NSTemporaryDirectory(), leaseID: nil,
                    taskName: "Review terminal"
                ))
            }
        }
        let view = ReviewTerminalView().environmentObject(model).environmentObject(vm)
        let startedAt = Date()
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 900, height: 540),
            styleMask: [.titled], backing: .buffered, defer: false
        )
        captureWindow.title = "Boss [terminal capture]"
        captureWindow.contentView = NSHostingView(rootView: view)
        window = captureWindow
        if state == "review-terminal-transition" {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) {
                terminalHost(in: captureWindow.contentView)?.submitText("pwd\n")
            }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + BossCaptureArgs.shared.captureAfter) {
            if state == "review-terminal-transition", case .idle = vm.state {
                fputs("terminal capture failed: state transition closed the terminal\n", stderr)
                exit(1)
            }
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                guard let host = terminalHost(in: captureWindow.contentView),
                      host.foregroundPid > 0, host.hasVisibleOutput else {
                    fputs("terminal capture failed: no live shell output\n", stderr)
                    exit(1)
                }
                fputs("terminal capture: live shell output after \(Int(Date().timeIntervalSince(startedAt) * 1000)) ms\n", stderr)
                NSApp.terminate(nil)
            } catch {
                fputs("terminal capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }

    private static func terminalHost(in view: NSView?) -> GhosttyTerminalHostView? {
        guard let view else { return nil }
        if let host = view as? GhosttyTerminalHostView { return host }
        for child in view.subviews {
            if let host = terminalHost(in: child) { return host }
        }
        return nil
    }
}
