import AppKit
import XCTest
@testable import Boss

/// Covers the App Nap incident (2026-07-15) diagnostics gap-closer: the
/// `DisplayPowerSample` JSON contract, its ring/JSONL-mirror wiring in
/// [[TerminalLoopLog]], and [[DisplayPowerMonitor]]'s notification
/// plumbing (start records, stop silences, start is idempotent). The App
/// Nap opt-out itself (`ProcessInfo.beginActivity` in `AppDelegate`) is a
/// single unconditional call with no branching lifecycle to unit test —
/// this suite instead pins the instrumentation half of the fix, which is
/// what a future incident's grep depends on.
final class DisplayPowerDiagnosticsTests: XCTestCase {

    // MARK: - JSON contract

    func testDisplayPowerSampleJSONHasKindAndSnakeCase() throws {
        let sample = DisplayPowerSample(tsEpochMs: 1_700_000_000_000, event: "sleep")
        let data = try JSONEncoder().encode(sample)
        let json = try XCTUnwrap(String(data: data, encoding: .utf8))
        XCTAssertTrue(json.contains("\"kind\":\"display_power\""))
        XCTAssertTrue(json.contains("\"ts_epoch_ms\""))
        XCTAssertTrue(json.contains("\"event\":\"sleep\""))

        let decoded = try JSONDecoder().decode(DisplayPowerSample.self, from: data)
        XCTAssertEqual(decoded, sample)
    }

    // MARK: - TerminalLoopLog wiring

    func testDisplayPowerRingAndSnapshotIndependentOfLoopsAndTabs() {
        let log = TerminalLoopLog(directory: nil)
        log.record(LoopSample(tsEpochMs: 1, wakeupsPerSec: 7, ticksPerSec: 0, intervalMs: 1000, panes: []))
        log.record(DisplayPowerSample(tsEpochMs: 2, event: "sleep"))
        log.record(DisplayPowerSample(tsEpochMs: 3, event: "wake"))

        XCTAssertEqual(log.loopSnapshot().count, 1)
        let snap = log.displayPowerSnapshot()
        XCTAssertEqual(snap.count, 2)
        XCTAssertEqual(snap.map(\.event), ["sleep", "wake"])
    }

    func testDisplayPowerRingEvictsOldestBeyondCapacity() {
        let log = TerminalLoopLog(directory: nil, capacity: 2)
        for i in 0..<4 {
            log.record(DisplayPowerSample(tsEpochMs: Int64(i), event: i % 2 == 0 ? "sleep" : "wake"))
        }
        let snap = log.displayPowerSnapshot()
        XCTAssertEqual(snap.count, 2)
        XCTAssertEqual(snap.first?.tsEpochMs, 2)
        XCTAssertEqual(snap.last?.tsEpochMs, 3)
    }

    // MARK: - DisplayPowerMonitor notification wiring

    func testMonitorRecordsSleepAndWakeFromNotifications() async {
        let log = TerminalLoopLog(directory: nil)
        let monitor = DisplayPowerMonitor(log: log)
        monitor.start()
        defer { monitor.stop() }

        let center = NSWorkspace.shared.notificationCenter
        let sleepSeen = XCTestExpectation(description: "sleep recorded")
        let wakeSeen = XCTestExpectation(description: "wake recorded")
        Task { @MainActor in
            while log.displayPowerSnapshot().count < 1 { await Task.yield() }
            sleepSeen.fulfill()
        }
        center.post(name: NSWorkspace.screensDidSleepNotification, object: nil)
        await fulfillment(of: [sleepSeen], timeout: 1.0)

        Task { @MainActor in
            while log.displayPowerSnapshot().count < 2 { await Task.yield() }
            wakeSeen.fulfill()
        }
        center.post(name: NSWorkspace.screensDidWakeNotification, object: nil)
        await fulfillment(of: [wakeSeen], timeout: 1.0)

        let snap = log.displayPowerSnapshot()
        XCTAssertEqual(snap.map(\.event), ["sleep", "wake"])
    }

    func testMonitorStopSilencesFurtherNotifications() async {
        let log = TerminalLoopLog(directory: nil)
        let monitor = DisplayPowerMonitor(log: log)
        monitor.start()
        monitor.stop()

        let center = NSWorkspace.shared.notificationCenter
        center.post(name: NSWorkspace.screensDidSleepNotification, object: nil)
        // Give the (now-unregistered) main-queue handler a beat it should
        // never take; a short async sleep is sufficient since the observer
        // was removed before the post.
        try? await Task.sleep(nanoseconds: 100_000_000)

        XCTAssertTrue(log.displayPowerSnapshot().isEmpty)
    }

    func testMonitorStartIsIdempotent() async {
        let log = TerminalLoopLog(directory: nil)
        let monitor = DisplayPowerMonitor(log: log)
        monitor.start()
        monitor.start()
        defer { monitor.stop() }

        let center = NSWorkspace.shared.notificationCenter
        let seen = XCTestExpectation(description: "sleep recorded exactly once")
        Task { @MainActor in
            while log.displayPowerSnapshot().isEmpty { await Task.yield() }
            // Give a duplicate registration a beat to double-fire before asserting.
            try? await Task.sleep(nanoseconds: 100_000_000)
            seen.fulfill()
        }
        center.post(name: NSWorkspace.screensDidSleepNotification, object: nil)
        await fulfillment(of: [seen], timeout: 1.0)

        XCTAssertEqual(log.displayPowerSnapshot().count, 1)
    }
}
