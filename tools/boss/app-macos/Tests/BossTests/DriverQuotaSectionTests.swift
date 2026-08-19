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
            guard case .unavailable(_, let reason) = entry.outcome else {
                return XCTFail("nothing should render as a reading before the first check")
            }
            XCTAssertEqual(reason, "not checked yet")
        }
    }

    // MARK: - Presentation

    func testWholePercentagesRenderWithoutASpuriousDecimal() {
        XCTAssertEqual(reading(percent: 3).usedPercentText, "3%")
        XCTAssertEqual(reading(percent: 0).usedPercentText, "0%")
        XCTAssertEqual(reading(percent: 12.5).usedPercentText, "12.5%")
    }

    func testProviderResetWordingIsUsedVerbatimWhenThereIsNoTimestamp() {
        var r = reading(percent: 3)
        r.resetsAtText = "Aug 25 at 7pm (America/Chicago)"
        XCTAssertEqual(
            r.resetsText(now: Date(timeIntervalSince1970: 1_787_000_000)),
            "resets Aug 25 at 7pm (America/Chicago)"
        )
    }

    func testNoResetInformationYieldsNoResetClauseRatherThanAnInventedOne() {
        XCTAssertNil(reading(percent: 3).resetsText(now: Date()))
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
