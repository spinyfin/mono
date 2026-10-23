import AppKit
import SwiftUI

/// Capture the real Metrics view after the normal app's cold connection.
/// Requires --capture-to, --capture-metrics, and the existing isolated launch.
@MainActor
enum BossMetricsCapture {
    private static var window: NSWindow?
    static var enabled: Bool {
        BossCaptureArgs.shared.isCaptureMode && CommandLine.arguments.contains("--capture-metrics")
    }
    // The command-line WindowGroup can fail to mount on macOS. Share the
    // app's actual model even when its view task never reaches the delegate.
    static let model = ChatViewModel(paths: BossEnginePaths.production())

    static func schedule(to path: String, after delay: TimeInterval) {
        guard BossEnginePaths.isIsolatedInstance else {
            fputs("metrics capture requires an isolated socket\n", stderr)
            exit(1)
        }
        model.startIfNeeded()
        // Exercise the real board Refresh action during an isolated traffic
        // capture, after the cold-start burst has settled.
        if CommandLine.arguments.contains("--capture-refresh-work") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 10) {
                model.refreshWork()
            }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
            let captureWindow = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 1080, height: 1000),
                styleMask: [.titled, .closable], backing: .buffered, defer: false
            )
            captureWindow.title = "Metrics [agent capture]"
            captureWindow.contentView = NSHostingView(rootView: MetricsViewer(filter: "frontend_")
                .environmentObject(model))
            window = captureWindow
            captureWindow.contentView?.layoutSubtreeIfNeeded()
            // An offscreen host need not receive SwiftUI's onAppear callback.
            model.refreshMetrics()
            Task { @MainActor in
                // Wait for the real metrics RPC, without opening another connection.
                for _ in 0..<50 {
                    if model.engineMetrics.contains(where: { $0.name == "frontend_requests_total" }) {
                        // Give SwiftUI a render turn after the snapshot arrives.
                        try? await Task.sleep(nanoseconds: 100_000_000)
                        do {
                            try BossWindowCapture.captureWindow(captureWindow, to: path)
                            for metric in model.engineMetrics where metric.name.hasPrefix("frontend_") {
                                print("\(metric.name): \(metric.value)")
                            }
                            NSApp.terminate(nil)
                            return
                        } catch {
                            fputs("metrics capture failed: \(error)\n", stderr)
                            exit(1)
                        }
                    }
                    try? await Task.sleep(nanoseconds: 100_000_000)
                }
                fputs("metrics capture did not receive frontend counters\n", stderr)
                exit(1)
            }
        }
    }
}
