import XCTest

@testable import Boss

/// Tests for the Engine pane's driver-quota rendering.
///
/// The property under test throughout is the one the feature exists for:
/// **a failed, missing, or stale reading must never render as a healthy
/// figure.** Each test below pins one way that could go wrong.
final class DriverQuotaSectionTests: XCTestCase {
    private func snapshot(_ json: [String: Any]) -> DriverQuotaSnapshot {
        DriverQuotaSnapshot.decode(json)
    }

    // MARK: - Decoding

    func testDecodesRealEngineSnapshot() {
        let decoded = snapshot([
            "entries": [
                [
                    "driver": "claude",
                    "observed_at_epoch_s": 1_787_000_000,
                    "outcome": [
                        "state": "reading",
                        "used_percent": 3.0,
                        "window": ["kind": "weekly"],
                        "resets_at_text": "Aug 25 at 7pm (America/Chicago)",
                        "source": "claude -p /usage",
                    ],
                ],
                [
                    "driver": "codex",
                    "observed_at_epoch_s": 1_787_000_001,
                    "outcome": ["state": "unavailable", "kind": "timeout", "reason": "no answer within 25s"],
                ],
            ],
            "generated_at_epoch_s": 1_787_000_002,
            "refresh_throttled": false,
        ])

        XCTAssertEqual(decoded.entries.count, 2)
        XCTAssertEqual(decoded.generatedAtEpochS, 1_787_000_002)
        guard case .reading(let reading) = decoded.entries[0].outcome else {
            return XCTFail("expected a reading for claude")
        }
        XCTAssertEqual(reading.usedPercent, 3.0)
        XCTAssertEqual(reading.window, .weekly)
        XCTAssertEqual(reading.source, "claude -p /usage")
        guard case .unavailable(let kind, _) = decoded.entries[1].outcome else {
            return XCTFail("expected a failure for codex")
        }
        XCTAssertEqual(kind, .timeout)
    }

    /// A malformed `reading` (no percentage) must be rejected outright.
    /// Half-decoding it into a zero is the exact failure mode this feature
    /// must not have.
    func testReadingWithoutAPercentageIsRejectedRatherThanTreatedAsZero() {
        let decoded = snapshot([
            "entries": [[
                "driver": "grok",
                "observed_at_epoch_s": 1,
                "outcome": ["state": "reading", "window": ["kind": "weekly"], "source": "x"],
            ]],
            "generated_at_epoch_s": 2,
        ])
        XCTAssertTrue(decoded.entries.isEmpty)
    }

    func testUnknownOutcomeStateIsDropped() {
        let decoded = snapshot([
            "entries": [["driver": "grok", "observed_at_epoch_s": 1, "outcome": ["state": "who_knows"]]],
        ])
        XCTAssertTrue(decoded.entries.isEmpty)
    }

    func testUnknownFailureKindFallsBackToAFailureNotToASuccess() {
        let decoded = snapshot([
            "entries": [[
                "driver": "grok",
                "observed_at_epoch_s": 1,
                "outcome": ["state": "unavailable", "kind": "brand_new_kind", "reason": "hmm"],
            ]],
        ])
        guard case .unavailable(let kind, _) = decoded.entries.first?.outcome else {
            return XCTFail("expected a failure")
        }
        XCTAssertEqual(kind, .probeFailed)
    }

    func testEmptyPayloadDecodesAsNeverChecked() {
        let decoded = snapshot([:])
        XCTAssertTrue(decoded.neverChecked)
        XCTAssertTrue(decoded.entries.isEmpty)
        XCTAssertFalse(decoded.refreshThrottled)
    }

    // MARK: - Roster

    func testRosterAlwaysCoversEveryDriverEvenWhenTheEngineOmitsOne() {
        let decoded = snapshot([
            "entries": [[
                "driver": "claude",
                "observed_at_epoch_s": 10,
                "outcome": ["state": "reading", "used_percent": 1, "window": ["kind": "weekly"], "source": "s"],
            ]],
            "generated_at_epoch_s": 11,
        ])
        let roster = DriverQuotaSection.roster(from: decoded)
        XCTAssertEqual(roster.map(\.driver), ["claude", "codex", "grok"])
        for slug in ["codex", "grok"] {
            let entry = roster.first { $0.driver == slug }
            guard case .unavailable = entry?.outcome else {
                return XCTFail("\(slug) must be shown as unavailable, never quietly skipped")
            }
        }
    }

    func testRosterOnAnEmptySnapshotSaysNotCheckedRatherThanFailed() {
        let roster = DriverQuotaSection.roster(from: .empty)
        XCTAssertEqual(roster.count, 3)
        for entry in roster {
            guard case .pending = entry.outcome else {
                return XCTFail("nothing should render as a reading or a failure before the first check")
            }
            XCTAssertEqual(entry.outcome.headline(checking: false), "Not checked yet")
            XCTAssertEqual(entry.outcome.headline(checking: true), "Checking…")
            XCTAssertFalse(entry.outcome.showsWarning, "not-yet-known must not share the failure glyph")
        }
    }

    // MARK: - Presentation

    func testWholePercentagesRenderWithoutASpuriousDecimal() {
        XCTAssertEqual(reading(percent: 3).usedPercentText, "3%")
        XCTAssertEqual(reading(percent: 0).usedPercentText, "0%")
        XCTAssertEqual(reading(percent: 12.5).usedPercentText, "12.5%")
    }

    func testProviderResetWordingIsUsedVerbatimOnlyAsAFallbackWithNoInstant() {
        var r = reading(percent: 3)
        r.resetsAtText = "some shape the parser did not recognise"
        XCTAssertEqual(
            r.resetsText(now: Date(timeIntervalSince1970: 1_787_000_000)),
            "resets some shape the parser did not recognise"
        )
    }

    func testNoResetInformationYieldsNoResetClauseRatherThanAnInventedOne() {
        XCTAssertNil(reading(percent: 3).resetsText(now: Date()))
    }

    /// All three drivers normalise to `resetsAtEpochS` at the engine's parse
    /// boundary; this view formats every one of them identically. Within the
    /// coming week: weekday name and local time, never the zone name.
    func testResetInstantWithinTheComingWeekUsesWeekdayAndTime() {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: "America/Chicago")!
        // "now" is a Monday; the reset instant is that Friday at 4:04 PM.
        let now = dateFrom("2026-08-17T09:00:00-05:00")
        let resets = dateFrom("2026-08-21T16:04:00-05:00")
        var r = reading(percent: 3)
        r.resetsAtEpochS = Int(resets.timeIntervalSince1970)
        XCTAssertEqual(r.resetsText(now: now, calendar: calendar), "resets Friday at 4:04 PM")
    }

    /// Beyond the coming week: a plain date, in one consistent style —
    /// never a raw ISO string and never sub-second precision.
    func testResetInstantBeyondTheComingWeekUsesAPlainDate() {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: "America/Chicago")!
        let now = dateFrom("2026-08-01T09:00:00-05:00")
        let resets = dateFrom("2026-08-25T18:59:00-05:00")
        var r = reading(percent: 3)
        r.resetsAtEpochS = Int(resets.timeIntervalSince1970)
        XCTAssertEqual(r.resetsText(now: now, calendar: calendar), "resets Aug 25, 2026")
    }

    private func dateFrom(_ iso: String) -> Date {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        return formatter.date(from: iso)!
    }

    func testNonWeeklyWindowIsNeverLabelledThisWeek() {
        XCTAssertEqual(DriverQuotaWindow.weekly.label, "this week")
        XCTAssertNotEqual(DriverQuotaWindow.other(minutes: 300).label, "this week")
        XCTAssertEqual(DriverQuotaWindow.other(minutes: 0).label, "current period")
    }

    func testNeverCheckedSnapshotSaysSoRatherThanShowingAnEpochZeroTime() {
        let text = DriverQuotaSnapshot.empty.checkedText(now: Date(timeIntervalSince1970: 1_787_000_000))
        XCTAssertEqual(text, "Not checked yet")
    }

    func testCheckedTextReportsAge() {
        var snap = DriverQuotaSnapshot.empty
        snap.generatedAtEpochS = 1_787_000_000
        XCTAssertEqual(
            snap.checkedText(now: Date(timeIntervalSince1970: 1_787_000_600)),
            "Checked 10 min ago"
        )
    }

    func testEveryFailureKindHasADistinguishableLabel() {
        let kinds: [DriverQuotaFailureKind] = [
            .notInstalled, .notAuthenticated, .probeFailed, .unparseable, .timeout,
        ]
        let labels = kinds.map(\.shortLabel)
        XCTAssertEqual(Set(labels).count, kinds.count, "failure kinds must not share a label")
        for label in labels {
            XCTAssertFalse(label.isEmpty)
            XCTAssertFalse(label.contains("0"), "a failure label must not read like a percentage")
            XCTAssertNotEqual(label, "—")
        }
    }

    private func reading(percent: Double) -> DriverQuotaReading {
        DriverQuotaReading(
            usedPercent: percent,
            window: .weekly,
            resetsAtEpochS: nil,
            resetsAtText: nil,
            source: "test"
        )
    }
}
