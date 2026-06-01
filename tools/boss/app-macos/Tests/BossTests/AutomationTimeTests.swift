import XCTest
@testable import Boss

/// Pins the "Next fire" / "Last fired" formatting in the automation
/// detail view. The engine serialises both `next_due_at` and
/// `last_fired_at` as UTC epoch seconds in a string (e.g. "1780295100");
/// rendering that raw value is unreadable, so the UI shows a relative
/// form with the absolute local time as a tooltip. These tests nail the
/// parse + format contract without hosting a SwiftUI view (mirrors
/// `WorkerStalenessTests`).
final class AutomationTimeTests: XCTestCase {

    func testParsesEpochSecondsString() {
        // 2026-06-01T12:00:00Z.
        let date = AutomationTime.parse("1780315200")
        XCTAssertEqual(date?.timeIntervalSince1970, 1780315200)
    }

    func testParseTrimsWhitespace() {
        XCTAssertEqual(
            AutomationTime.parse("  1780315200 ")?.timeIntervalSince1970,
            1780315200
        )
    }

    func testParseFallsBackToRFC3339() {
        XCTAssertNotNil(AutomationTime.parse("2026-06-01T12:00:00Z"))
        XCTAssertNotNil(AutomationTime.parse("2026-06-01T12:00:00.123Z"))
    }

    func testParseReturnsNilForEmptyOrGarbage() {
        XCTAssertNil(AutomationTime.parse(""))
        XCTAssertNil(AutomationTime.parse("   "))
        XCTAssertNil(AutomationTime.parse("not-a-timestamp"))
    }

    func testRelativeRendersFutureFire() {
        let now = Date(timeIntervalSince1970: 1780315200)
        // 21 minutes later.
        let raw = String(Int64(1780315200 + 21 * 60))
        let rendered = AutomationTime.relative(raw, now: now)
        XCTAssertTrue(
            rendered.localizedCaseInsensitiveContains("minute"),
            "expected a minutes-based relative string, got \(rendered)"
        )
        // Must not leak the raw epoch through.
        XCTAssertFalse(rendered.contains(raw))
    }

    func testRelativeRendersPastFire() {
        let now = Date(timeIntervalSince1970: 1780315200)
        // 2 hours earlier.
        let raw = String(Int64(1780315200 - 2 * 60 * 60))
        let rendered = AutomationTime.relative(raw, now: now)
        XCTAssertTrue(
            rendered.localizedCaseInsensitiveContains("hour"),
            "expected an hours-based relative string, got \(rendered)"
        )
        XCTAssertFalse(rendered.contains(raw))
    }

    func testRelativeFallsBackToRawWhenUnparseable() {
        XCTAssertEqual(
            AutomationTime.relative("not-a-timestamp", now: Date()),
            "not-a-timestamp"
        )
    }

    func testAbsoluteRendersLocalTimeAndIsNotRawEpoch() {
        let raw = "1780315200"
        let absolute = AutomationTime.absolute(raw)
        XCTAssertNotNil(absolute)
        XCTAssertNotEqual(absolute, raw)
        // A formatted date carries a year; the raw epoch does not.
        XCTAssertTrue(absolute!.contains("2026"))
    }

    func testAbsoluteReturnsNilForGarbage() {
        XCTAssertNil(AutomationTime.absolute("not-a-timestamp"))
        XCTAssertNil(AutomationTime.absolute(""))
    }
}
