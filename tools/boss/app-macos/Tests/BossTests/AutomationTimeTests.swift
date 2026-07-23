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

    // MARK: - nextFire

    func testNextFireRendersRelativeWhenUpcoming() {
        let now = Date(timeIntervalSince1970: 1780315200)
        let raw = String(Int64(1780315200 + 18 * 60))
        let rendered = AutomationTime.nextFire(raw, now: now, paused: false)
        XCTAssertTrue(
            rendered.localizedCaseInsensitiveContains("minute"),
            "expected a minutes-based relative string, got \(rendered)"
        )
    }

    /// While globally paused the scheduler evaluates nothing, so the parked
    /// `next_due_at` is not a prediction — reporting "in 18 minutes" for an
    /// automation that cannot fire until a human resumes is the bug.
    func testNextFireSaysPausedRegardlessOfTimestamp() {
        let now = Date(timeIntervalSince1970: 1780315200)
        let upcoming = String(Int64(1780315200 + 18 * 60))
        let elapsed = String(Int64(1780315200 - 3 * 60 * 60))
        XCTAssertEqual(AutomationTime.nextFire(upcoming, now: now, paused: true), "Paused")
        XCTAssertEqual(AutomationTime.nextFire(elapsed, now: now, paused: true), "Paused")
    }

    /// An un-advanced occurrence is still pending, not something that already
    /// happened — "2 hours ago" under a "Next fire" label reads as a bug.
    func testNextFireSaysDueNowForAnUnconsumedOccurrence() {
        let now = Date(timeIntervalSince1970: 1780315200)
        let elapsed = String(Int64(1780315200 - 2 * 60 * 60))
        XCTAssertEqual(AutomationTime.nextFire(elapsed, now: now, paused: false), "Due now")
        // Exactly now counts as due, not as an 18-minutes-hence prediction.
        XCTAssertEqual(AutomationTime.nextFire("1780315200", now: now, paused: false), "Due now")
    }

    func testNextFireFallsBackToRawWhenUnparseable() {
        XCTAssertEqual(
            AutomationTime.nextFire("not-a-timestamp", now: Date(), paused: false),
            "not-a-timestamp"
        )
    }

    // MARK: - run outcome labels

    /// `repeat_count` collapses consecutive same-outcome rows, and each row is
    /// one distinct cron occurrence — the engine upserts on
    /// `(automation_id, scheduled_for)`. It was never an attempt counter, so
    /// the label must not call it a retry count.
    func testOutcomeLabelCountsOccurrencesNotRetries() {
        var run = AppAutomationRun(
            id: "autorun_1",
            automationID: "auto_1",
            scheduledFor: "1780315200",
            startedAt: "1780315200",
            outcome: "failed_will_retry"
        )
        XCTAssertEqual(run.outcomeLabel, "Failed (retrying)")

        run.repeatCount = 22
        XCTAssertEqual(run.outcomeLabel, "Failed (retrying), 22 occurrences")
        XCTAssertFalse(
            run.outcomeLabel.localizedCaseInsensitiveContains("retried"),
            "must not present an occurrence count as a retry count"
        )
    }
}
