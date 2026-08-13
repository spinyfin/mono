import Foundation
import XCTest
@testable import Boss

/// Pins the begin/end accounting behind [[MenuTrackingMonitor]].
///
/// The monitor exists to distinguish "a menu is genuinely open" from "a nested
/// menu tracking loop outlived its menu" — a distinction `sample` cannot draw.
/// These tests drive the ledger directly: opening a real `NSMenu` enters
/// AppKit's nested modal event loop, which a test process has no deterministic
/// way back out of.
final class MenuTrackingLedgerTests: XCTestCase {
    func testMatchedBeginEndReportsDuration() {
        var ledger = MenuTrackingLedger()
        XCTAssertFalse(ledger.isTracking)
        XCTAssertTrue(ledger.begin(key: 1, label: "Group by", atMs: 1_000))
        XCTAssertTrue(ledger.isTracking)
        XCTAssertEqual(ledger.openCount, 1)

        let closed = ledger.end(key: 1, atMs: 2_500)
        XCTAssertEqual(closed, ClosedMenuSession(label: "Group by", openMs: 1_500))
        XCTAssertFalse(ledger.isTracking)
        XCTAssertEqual(ledger.unbalancedEndCount, 0)
    }

    /// A submenu posts its own begin/end pair inside its parent's. That is
    /// normal AppKit behaviour, not a leak, so both must stay tracked and the
    /// parent must survive the submenu closing.
    func testNestedSubmenuSessionsAreTrackedIndependently() {
        var ledger = MenuTrackingLedger()
        ledger.begin(key: 1, label: "Board Style", atMs: 0)
        ledger.begin(key: 2, label: "Density", atMs: 400)
        XCTAssertEqual(ledger.openCount, 2)

        XCTAssertEqual(ledger.end(key: 2, atMs: 900)?.openMs, 500)
        XCTAssertEqual(ledger.openCount, 1)
        XCTAssertTrue(ledger.isTracking)

        XCTAssertEqual(ledger.end(key: 1, atMs: 1_200)?.openMs, 1_200)
        XCTAssertFalse(ledger.isTracking)
    }

    /// AppKit can post a second begin for a menu that is re-shown without an
    /// intervening end. Counting it twice would leave a phantom session open
    /// forever after the real one closes — the exact false positive this
    /// monitor is supposed to rule out.
    func testDuplicateBeginIsIgnored() {
        var ledger = MenuTrackingLedger()
        XCTAssertTrue(ledger.begin(key: 7, label: "New", atMs: 100))
        XCTAssertFalse(ledger.begin(key: 7, label: "New", atMs: 250))
        XCTAssertEqual(ledger.openCount, 1)
        XCTAssertEqual(ledger.end(key: 7, atMs: 300)?.openMs, 200)
        XCTAssertEqual(ledger.openCount, 0)
    }

    func testEndWithoutBeginIsCountedNotDropped() {
        var ledger = MenuTrackingLedger()
        XCTAssertNil(ledger.end(key: 42, atMs: 10))
        XCTAssertEqual(ledger.unbalancedEndCount, 1)
        XCTAssertFalse(ledger.isTracking)
    }

    func testLongestOpenPicksTheEarliestSession() {
        var ledger = MenuTrackingLedger()
        ledger.begin(key: 1, label: "first", atMs: 1_000)
        ledger.begin(key: 2, label: "second", atMs: 5_000)
        XCTAssertEqual(ledger.longestOpen(asOfMs: 6_000)?.label, "first")
        XCTAssertEqual(ledger.longestOpen(asOfMs: 6_000)?.openMs(asOfMs: 6_000), 5_000)
    }

    // MARK: - Report backoff

    func testReportsEscalateThenSettleToTheSteadyInterval() {
        var ledger = MenuTrackingLedger()
        ledger.begin(key: 1, label: "stuck", atMs: 0)

        // Below the first tier: nothing to say yet.
        XCTAssertTrue(ledger.dueReports(asOfMs: 900).isEmpty)

        var reportedAt: [Int64] = []
        // Tick once a second for two minutes, as the live probe does.
        for second in 1...120 {
            let now = Int64(second) * 1_000
            for due in ledger.dueReports(asOfMs: now) {
                XCTAssertEqual(due.label, "stuck")
                reportedAt.append(due.openMs)
            }
        }

        // 1s, 2s, 5s, 10s, 30s, then one per steady interval (90s).
        XCTAssertEqual(reportedAt, [1_000, 2_000, 5_000, 10_000, 30_000, 90_000])
    }

    /// A tick that skips several tiers at once (the main thread was busy)
    /// must emit one record, not one per skipped tier.
    func testASingleLateTickCollapsesSkippedTiers() {
        var ledger = MenuTrackingLedger()
        ledger.begin(key: 1, label: "stuck", atMs: 0)
        let due = ledger.dueReports(asOfMs: 12_000)
        XCTAssertEqual(due.count, 1)
        XCTAssertEqual(due.first?.openMs, 12_000)
        // The next report is the 30s tier, not a replay of the skipped ones.
        XCTAssertTrue(ledger.dueReports(asOfMs: 20_000).isEmpty)
        XCTAssertEqual(ledger.dueReports(asOfMs: 31_000).count, 1)
    }

    func testThresholdsPastTheLastTierStepBySteadyInterval() {
        let last = MenuTrackingLedger.reportTiersMs.count - 1
        XCTAssertEqual(
            MenuTrackingLedger.thresholdMs(forTier: last),
            MenuTrackingLedger.reportTiersMs[last]
        )
        XCTAssertEqual(
            MenuTrackingLedger.thresholdMs(forTier: last + 1),
            MenuTrackingLedger.reportTiersMs[last] + MenuTrackingLedger.steadyIntervalMs
        )
        XCTAssertEqual(
            MenuTrackingLedger.thresholdMs(forTier: last + 3),
            MenuTrackingLedger.reportTiersMs[last] + MenuTrackingLedger.steadyIntervalMs * 3
        )
    }

    // MARK: - Labelling

    func testTitleWinsWhenPresent() {
        XCTAssertEqual(
            MenuTrackingLedger.label(title: "Board Style", itemTitles: ["Compact", "Roomy"]),
            "Board Style"
        )
    }

    /// SwiftUI's `Menu` bridge leaves `NSMenu.title` empty, so the item titles
    /// are the only thing that names the offending control. This is the case
    /// the investigation actually needs to work.
    func testUntitledMenuIsNamedByItsItems() {
        let label = MenuTrackingLedger.label(
            title: "",
            itemTitles: ["New Product", "New Project", "New Task", "New Chore"]
        )
        XCTAssertEqual(label, "New Product / New Project / New Task / New Chore")
    }

    func testLongItemListIsCappedAndMarkedElided() {
        let label = MenuTrackingLedger.label(
            title: "   ",
            itemTitles: (1...20).map { "Turn \($0)" }
        )
        XCTAssertEqual(label, "Turn 1 / Turn 2 / Turn 3 / Turn 4 / …")
    }

    func testOverlongItemTitleIsTruncated() {
        let long = String(repeating: "x", count: 100)
        let label = MenuTrackingLedger.label(title: "", itemTitles: [long])
        XCTAssertEqual(label.count, MenuTrackingLedger.maxLabelItemLength + 1)
        XCTAssertTrue(label.hasSuffix("…"))
    }

    func testEmptyMenuStillProducesAName() {
        XCTAssertEqual(MenuTrackingLedger.label(title: "", itemTitles: []), "(untitled, no items)")
        XCTAssertEqual(MenuTrackingLedger.label(title: "", itemTitles: ["", "  "]), "(untitled, no items)")
    }
}
