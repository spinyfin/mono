import AppKit
import SwiftUI

/// Capture the Token Usage window after the isolated app's cold connection.
/// Requires `--capture-to`, `--capture-token-usage`, and an isolated socket.
/// Optional `--capture-token-usage-scene=tree|grouped|capture-boundary|inspector|
/// over-time|over-time-grouped|over-time-all|over-time-hover` selects which
/// screenshot to take; default is `tree`. The Over time scenes use a taller
/// window so all five cards fit without scrolling.
@MainActor
enum TokenUsageCapture {
    private static var window: NSWindow?
    static var enabled: Bool {
        BossCaptureArgs.shared.isCaptureMode && CommandLine.arguments.contains("--capture-token-usage")
    }

    // The command-line WindowGroup can fail to mount on macOS. Share the
    // app's actual model even when its view task never reaches the delegate.
    static let model = ChatViewModel(paths: BossEnginePaths.production())

    static var scene: TokenUsageCaptureScene {
        parseScene(from: CommandLine.arguments) ?? .tree
    }

    nonisolated static func parseScene(from arguments: [String]) -> TokenUsageCaptureScene? {
        var i = 1
        while i < arguments.count {
            let arg = arguments[i]
            if arg == "--capture-token-usage-scene", i + 1 < arguments.count {
                return TokenUsageCaptureScene(rawValue: arguments[i + 1])
            }
            if arg.hasPrefix("--capture-token-usage-scene=") {
                let value = String(arg.dropFirst("--capture-token-usage-scene=".count))
                return TokenUsageCaptureScene(rawValue: value)
            }
            i += 1
        }
        return nil
    }

    static func schedule(to path: String, after delay: TimeInterval) {
        guard BossEnginePaths.isIsolatedInstance else {
            fputs("token-usage capture requires an isolated socket\n", stderr)
            exit(1)
        }
        model.startIfNeeded()
        let captureScene = Self.scene
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
            let height: CGFloat = captureScene.isOverTime ? 1200 : 720
            let captureWindow = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 1180, height: height),
                styleMask: [.titled, .closable, .resizable],
                backing: .buffered,
                defer: false
            )
            captureWindow.title = "Token Usage [agent capture]"
            captureWindow.contentView = NSHostingView(
                rootView: TokenUsageWindow(captureScene: captureScene)
                    .environmentObject(model)
            )
            window = captureWindow
            captureWindow.contentView?.layoutSubtreeIfNeeded()
            Task { @MainActor in
                for _ in 0..<150 {
                    let ready = captureScene.isOverTime
                        ? model.tokenUsageSeries.report != nil
                        : model.tokenUsage.report != nil
                    if ready {
                        try? await Task.sleep(nanoseconds: 250_000_000)
                        expandOutlines(in: captureWindow.contentView)
                        // Charts lay out a beat after the data arrives.
                        try? await Task.sleep(nanoseconds: captureScene.isOverTime ? 1_000_000_000 : 150_000_000)
                        do {
                            try BossWindowCapture.captureWindow(captureWindow, to: path)
                            NSApp.terminate(nil)
                            return
                        } catch {
                            fputs("token-usage capture failed: \(error)\n", stderr)
                            exit(1)
                        }
                    }
                    let failure = captureScene.isOverTime ? model.tokenUsageSeries.error : model.tokenUsage.error
                    if let error = failure, !ready {
                        fputs("token-usage capture failed: \(error)\n", stderr)
                        exit(1)
                    }
                    try? await Task.sleep(nanoseconds: 100_000_000)
                }
                fputs("token-usage capture did not receive a usage reply\n", stderr)
                exit(1)
            }
        }
    }

    static func expandOutlines(in view: NSView?) {
        guard let view else { return }
        if let outline = view as? NSOutlineView {
            outline.expandItem(nil, expandChildren: true)
            return
        }
        for sub in view.subviews {
            expandOutlines(in: sub)
        }
    }
}
