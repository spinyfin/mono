import AppKit
import Foundation
import os

/// Observes display sleep/wake and mirrors each transition into
/// [[TerminalLoopLog]] as a [[DisplayPowerSample]].
///
/// Added after the 2026-07-15 App Nap incident (display off 18:48:44 -
/// 18:53:32 triggered 24-87s engine RPC ack delays, tripping the
/// SpawnHealthTracker breaker for ~10 hours). Before this monitor the app
/// kept no record of display power state at all, so confirming "was the
/// display asleep, and for how long" required external forensics (`pmset`
/// logs, coordinator-side timeline reconstruction) instead of a grep over
/// the app's own diagnostics stream. This is instrumentation only — the
/// App Nap opt-out itself lives in `AppDelegate` (`ProcessInfo.beginActivity`).
///
/// `NSWorkspace.screensDidSleepNotification` / `screensDidWakeNotification`
/// specifically track *display* power (independent of full-system sleep),
/// which is what throttled the run loop in the incident — distinct from
/// `GhosttyRuntime`'s wake-only observer, which exists to retry surface
/// creation after a wake, not to record power state.
final class DisplayPowerMonitor: @unchecked Sendable {
    static let shared = DisplayPowerMonitor()

    private let log: TerminalLoopLog
    private let logger = Logger(subsystem: "com.boss.app", category: "display-power")
    private var sleepObserver: NSObjectProtocol?
    private var wakeObserver: NSObjectProtocol?

    init(log: TerminalLoopLog = .shared) {
        self.log = log
    }

    /// Start observing display sleep/wake. Idempotent; call once at launch
    /// (next to `TerminalLoopMonitor.shared.start()`).
    func start() {
        guard sleepObserver == nil else { return }
        let center = NSWorkspace.shared.notificationCenter
        sleepObserver = center.addObserver(
            forName: NSWorkspace.screensDidSleepNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in self?.record(event: "sleep") }
        wakeObserver = center.addObserver(
            forName: NSWorkspace.screensDidWakeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in self?.record(event: "wake") }
    }

    func stop() {
        let center = NSWorkspace.shared.notificationCenter
        if let sleepObserver { center.removeObserver(sleepObserver) }
        if let wakeObserver { center.removeObserver(wakeObserver) }
        sleepObserver = nil
        wakeObserver = nil
    }

    private func record(event: String) {
        let sample = DisplayPowerSample(
            tsEpochMs: Int64(Date().timeIntervalSince1970 * 1000),
            event: event
        )
        log.record(sample)
        logger.notice("display-power: \(event, privacy: .public)")
    }
}
