import AppKit
import SwiftUI

/// Quiet Settings quota fixture, using the captured agy provider reading.
@MainActor
enum BossDriverQuotaCapture {
    private static var window: NSWindow?

    static func start(path: String) {
        guard BossCaptureArgs.shared.isCaptureMode, BossEnginePaths.isIsolatedInstance else { return }
        let model = ChatViewModel(paths: BossEnginePaths.production())
        let now = Int(Date().timeIntervalSince1970)
        model.driverQuota = DriverQuotaSnapshot(entries: [
            DriverQuotaEntry(driver: "agy", observedAtEpochS: now, outcome: .reading(DriverQuotaReading(
                usedPercent: 0.84230303764343, window: .weekly, resetsAtEpochS: 1795567941,
                source: "agy --print /usage (Gemini weekly)"
            ))),
        ], generatedAtEpochS: nil, pendingDrivers: ["claude", "codex", "grok"])
        let captureWindow = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 640, height: 440),
            styleMask: [.titled], backing: .buffered, defer: false
        )
        captureWindow.title = "Boss [agent capture]"
        captureWindow.contentView = NSHostingView(rootView:
            Form { DriverQuotaSection() }.formStyle(.grouped).environmentObject(model))
        window = captureWindow
        captureWindow.contentView?.layoutSubtreeIfNeeded()
        DispatchQueue.main.asyncAfter(deadline: .now() + BossCaptureArgs.shared.captureAfter) {
            do {
                try BossWindowCapture.captureWindow(captureWindow, to: path)
                NSApp.terminate(nil)
            } catch {
                fputs("driver quota capture failed: \(error)\n", stderr)
                exit(1)
            }
        }
    }
}
