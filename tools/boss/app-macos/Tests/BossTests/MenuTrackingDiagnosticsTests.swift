import AppKit
import XCTest
@testable import Boss

/// Covers the [[MenuTrackingSample]] JSON contract, its ring/JSONL-mirror
/// wiring in [[TerminalLoopLog]], and [[MenuTrackingMonitor]]'s notification
/// plumbing.
///
/// `NSMenu.didBeginTrackingNotification` / `didEndTrackingNotification` can be
/// posted directly with a real `NSMenu` as the object, so the monitor's whole
/// begin→end path is exercised here without entering AppKit's nested modal
/// menu loop — which is the one thing a unit test cannot get back out of.
@MainActor
final class MenuTrackingDiagnosticsTests: XCTestCase {

    // MARK: - JSON contract

    func testMenuTrackingSampleJSONHasKindAndSnakeCase() throws {
        let sample = MenuTrackingSample(
            tsEpochMs: 1_700_000_000_000,
            event: "still_open",
            menu: "New Product / New Project",
            openMs: 4_200,
            openCount: 2,
            runloopMode: "NSEventTrackingRunLoopMode"
        )
        let data = try JSONEncoder().encode(sample)
        let json = try XCTUnwrap(String(data: data, encoding: .utf8))
        XCTAssertTrue(json.contains("\"kind\":\"menu_tracking\""), json)
        XCTAssertTrue(json.contains("\"ts_epoch_ms\""), json)
        XCTAssertTrue(json.contains("\"open_ms\":4200"), json)
        XCTAssertTrue(json.contains("\"open_count\":2"), json)
        XCTAssertTrue(json.contains("\"runloop_mode\":\"NSEventTrackingRunLoopMode\""), json)
        XCTAssertFalse(json.contains("openMs"), json)

        let decoded = try JSONDecoder().decode(MenuTrackingSample.self, from: data)
        XCTAssertEqual(decoded, sample)
    }

    // MARK: - TerminalLoopLog wiring

    func testMenuTrackingRingIsIndependentOfTheOtherStreams() {
        let log = TerminalLoopLog(directory: nil)
        log.record(DisplayPowerSample(tsEpochMs: 1, event: "sleep"))
        log.record(sample(event: "begin", at: 2))
        log.record(sample(event: "end", at: 3))

        XCTAssertEqual(log.displayPowerSnapshot().count, 1)
        XCTAssertEqual(log.menuTrackingSnapshot().map(\.event), ["begin", "end"])
    }

    func testMenuTrackingRingEvictsOldestBeyondCapacity() {
        let log = TerminalLoopLog(directory: nil, capacity: 2)
        for i in 0..<4 {
            log.record(sample(event: "still_open", at: Int64(i)))
        }
        let snap = log.menuTrackingSnapshot()
        XCTAssertEqual(snap.count, 2)
        XCTAssertEqual(snap.map(\.tsEpochMs), [2, 3])
    }

    // MARK: - Monitor notification wiring

    func testMonitorRecordsBeginAndEndWithADerivedLabel() async {
        let log = TerminalLoopLog(directory: nil)
        let monitor = MenuTrackingMonitor(log: log)
        monitor.start()
        defer { monitor.stop() }

        let menu = untitledMenu(items: ["New Product", "New Project"])
        post(.didBeginTracking, menu)
        await waitForSamples(log, count: 1)
        XCTAssertTrue(monitor.isTracking)
        XCTAssertEqual(monitor.openSessions().map(\.label), ["New Product / New Project"])

        post(.didEndTracking, menu)
        await waitForSamples(log, count: 2)
        XCTAssertFalse(monitor.isTracking)

        let snap = log.menuTrackingSnapshot()
        XCTAssertEqual(snap.map(\.event), ["begin", "end"])
        XCTAssertEqual(snap.map(\.menu), ["New Product / New Project", "New Product / New Project"])
        XCTAssertNil(snap[0].openMs)
        XCTAssertNotNil(snap[1].openMs)
        XCTAssertEqual(snap[0].openCount, 1)
        XCTAssertEqual(snap[1].openCount, 0)
    }

    /// The load-bearing case: an end that never arrives leaves the session
    /// open and named, so a profile showing a nested menu loop can be
    /// matched to the control that owns it.
    func testSessionWithNoEndStaysOpenAndNamed() async {
        let log = TerminalLoopLog(directory: nil)
        let monitor = MenuTrackingMonitor(log: log)
        monitor.start()
        defer { monitor.stop() }

        post(.didBeginTracking, untitledMenu(items: ["Group by status"]))
        await waitForSamples(log, count: 1)

        XCTAssertTrue(monitor.isTracking)
        let open = monitor.openSessions()
        XCTAssertEqual(open.count, 1)
        XCTAssertEqual(open.first?.label, "Group by status")
    }

    func testEndWithoutBeginIsRecordedRatherThanDropped() async {
        let log = TerminalLoopLog(directory: nil)
        let monitor = MenuTrackingMonitor(log: log)
        monitor.start()
        defer { monitor.stop() }

        post(.didEndTracking, untitledMenu(items: ["Copy"]))
        await waitForSamples(log, count: 1)

        XCTAssertEqual(log.menuTrackingSnapshot().map(\.event), ["unbalanced_end"])
        XCTAssertEqual(log.menuTrackingSnapshot().first?.firstUnbalancedEnd, true)
        XCTAssertFalse(monitor.isTracking)
    }

    func testTitledMenuKeepsItsTitle() async {
        let log = TerminalLoopLog(directory: nil)
        let monitor = MenuTrackingMonitor(log: log)
        monitor.start()
        defer { monitor.stop() }

        let menu = NSMenu(title: "Board Style")
        menu.addItem(NSMenuItem(title: "Compact", action: nil, keyEquivalent: ""))
        post(.didBeginTracking, menu)
        await waitForSamples(log, count: 1)

        XCTAssertEqual(log.menuTrackingSnapshot().first?.menu, "Board Style")
    }

    func testStopSilencesFurtherNotifications() async {
        let log = TerminalLoopLog(directory: nil)
        let monitor = MenuTrackingMonitor(log: log)
        monitor.start()
        monitor.stop()

        post(.didBeginTracking, untitledMenu(items: ["New Task"]))
        try? await Task.sleep(nanoseconds: 100_000_000)

        XCTAssertTrue(log.menuTrackingSnapshot().isEmpty)
        XCTAssertFalse(monitor.isTracking)
    }

    /// Stopping mid-session must not leave `isTracking` asserting that a menu
    /// is open which the monitor can no longer see close.
    func testStopClearsAnOpenSession() async {
        let log = TerminalLoopLog(directory: nil)
        let monitor = MenuTrackingMonitor(log: log)
        monitor.start()

        post(.didBeginTracking, untitledMenu(items: ["New Task"]))
        await waitForSamples(log, count: 1)
        XCTAssertTrue(monitor.isTracking)

        monitor.stop()
        XCTAssertFalse(monitor.isTracking)
        XCTAssertTrue(monitor.openSessions().isEmpty)
    }

    func testStartIsIdempotent() async {
        let log = TerminalLoopLog(directory: nil)
        let monitor = MenuTrackingMonitor(log: log)
        monitor.start()
        monitor.start()
        defer { monitor.stop() }

        post(.didBeginTracking, untitledMenu(items: ["New Task"]))
        await waitForSamples(log, count: 1)
        // Give a duplicate registration a beat to double-fire before asserting.
        try? await Task.sleep(nanoseconds: 100_000_000)

        XCTAssertEqual(log.menuTrackingSnapshot().count, 1)
    }

    // MARK: - Helpers

    private enum MenuNotification {
        case didBeginTracking
        case didEndTracking

        var name: Notification.Name {
            switch self {
            case .didBeginTracking: return NSMenu.didBeginTrackingNotification
            case .didEndTracking: return NSMenu.didEndTrackingNotification
            }
        }
    }

    private func post(_ notification: MenuNotification, _ menu: NSMenu) {
        NotificationCenter.default.post(name: notification.name, object: menu)
    }

    /// SwiftUI's `Menu` bridge produces exactly this shape: an `NSMenu` with
    /// no title whose items carry the only identifying text.
    private func untitledMenu(items: [String]) -> NSMenu {
        let menu = NSMenu(title: "")
        for item in items {
            menu.addItem(NSMenuItem(title: item, action: nil, keyEquivalent: ""))
        }
        return menu
    }

    private func waitForSamples(_ log: TerminalLoopLog, count: Int) async {
        let seen = XCTestExpectation(description: "\(count) menu-tracking sample(s) recorded")
        Task { @MainActor in
            while log.menuTrackingSnapshot().count < count { await Task.yield() }
            seen.fulfill()
        }
        await fulfillment(of: [seen], timeout: 2.0)
    }

    private func sample(event: String, at tsEpochMs: Int64) -> MenuTrackingSample {
        MenuTrackingSample(
            tsEpochMs: tsEpochMs,
            event: event,
            menu: "menu",
            openMs: nil,
            openCount: 0,
            runloopMode: nil
        )
    }
}
