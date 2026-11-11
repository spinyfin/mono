import XCTest
@testable import Boss

/// Covers the Settings driver-traffic control's editing model. The engine
/// rejects any split whose shares do not sum to exactly 100 — it never
/// repairs one — so the UI's contract is that `adjusting(_:to:)` can only
/// ever produce a valid split, and that every valid split is reachable
/// through it without passing through a rejected intermediate.
final class DriverTrafficSplitTests: XCTestCase {
    /// The shipped starting point: everything on the engine's default
    /// driver, nothing on Grok or Antigravity.
    func testEngineDefaultIsAllClaude() {
        let split = DriverTrafficSplit.engineDefault
        XCTAssertEqual(split.grok, 0)
        XCTAssertEqual(split.claude, 100)
        XCTAssertEqual(split.codex, 0)
        XCTAssertEqual(split.agy, 0, "Antigravity must receive no traffic by default")
        XCTAssertTrue(split.isValid)
    }

    /// Raising a share drains Claude first — the engine's default driver
    /// holds the traffic nobody has deliberately claimed.
    func testRaisingAShareTakesFromClaudeFirst() {
        let split = DriverTrafficSplit.engineDefault.adjusting(.grok, to: 20)
        XCTAssertEqual(split, DriverTrafficSplit(grok: 20, claude: 80, codex: 0, agy: 0))
    }

    /// Once Claude is exhausted the next donor gives way, and the result is
    /// still exactly 100.
    func testRaisingPastClaudeDrainsTheNextDonor() {
        let start = DriverTrafficSplit(grok: 20, claude: 30, codex: 50, agy: 0)
        let split = start.adjusting(.grok, to: 90)
        XCTAssertEqual(split, DriverTrafficSplit(grok: 90, claude: 0, codex: 10, agy: 0))
    }

    /// Lowering a share hands the whole surplus to the first donor.
    func testLoweringAShareGivesBackToTheFirstDonor() {
        let start = DriverTrafficSplit(grok: 40, claude: 30, codex: 30, agy: 0)
        XCTAssertEqual(start.adjusting(.grok, to: 10), DriverTrafficSplit(grok: 10, claude: 60, codex: 30, agy: 0))
        // Lowering Claude itself has to go somewhere: Codex before Grok, so
        // Grok only ever receives traffic the operator explicitly gave it.
        XCTAssertEqual(start.adjusting(.claude, to: 0), DriverTrafficSplit(grok: 40, claude: 0, codex: 60, agy: 0))
    }

    /// A driver can be taken to 100, zeroing the other three in one edit.
    func testRaisingToOneHundredZeroesTheOthers() {
        for driver in DriverSlug.allCases {
            let split = DriverTrafficSplit(grok: 33, claude: 33, codex: 34, agy: 0).adjusting(driver, to: 100)
            XCTAssertEqual(split.share(for: driver), 100, "\(driver)")
            XCTAssertTrue(split.isValid, "\(driver)")
            for other in DriverSlug.allCases where other != driver {
                XCTAssertEqual(split.share(for: other), 0, "\(other) under \(driver) at 100")
            }
        }
    }

    /// Every valid split is reachable, and no step along the way is invalid.
    /// Three single-share edits are enough for any target: Claude takes the
    /// remainder after setting Grok, Antigravity, and Codex.
    func testEveryValidSplitIsReachableWithoutAnInvalidIntermediate() {
        for grok in stride(from: 0, through: 100, by: 5) {
            for claude in stride(from: 0, through: 100 - grok, by: 5) {
                for agy in stride(from: 0, through: 100 - grok - claude, by: 5) {
                    let codex = 100 - grok - claude - agy
                    let first = DriverTrafficSplit.engineDefault.adjusting(.grok, to: grok)
                    XCTAssertTrue(first.isValid, "intermediate \(first) is invalid")
                    let second = first.adjusting(.agy, to: agy)
                    XCTAssertTrue(second.isValid, "intermediate \(second) is invalid")
                    let third = second.adjusting(.codex, to: codex)
                    XCTAssertTrue(third.isValid, "intermediate \(third) is invalid")
                    XCTAssertEqual(third, DriverTrafficSplit(grok: grok, claude: claude, codex: codex, agy: agy))
                }
            }
        }
    }

    /// Out-of-range input from a stepper is clamped into range and still
    /// yields a valid split — the sum invariant never depends on the caller.
    func testOutOfRangeValuesAreClampedAndStillSumToOneHundred() {
        let low = DriverTrafficSplit(grok: 40, claude: 30, codex: 30, agy: 0).adjusting(.grok, to: -25)
        XCTAssertEqual(low, DriverTrafficSplit(grok: 0, claude: 70, codex: 30, agy: 0))
        let high = DriverTrafficSplit(grok: 40, claude: 30, codex: 30, agy: 0).adjusting(.grok, to: 250)
        XCTAssertEqual(high, DriverTrafficSplit(grok: 100, claude: 0, codex: 0, agy: 0))
    }

    /// Setting a share to what it already is is a no-op, so the view model's
    /// change guard suppresses a pointless round trip.
    func testAdjustingToTheCurrentValueIsANoOp() {
        let split = DriverTrafficSplit(grok: 40, claude: 30, codex: 30, agy: 0)
        XCTAssertEqual(split.adjusting(.claude, to: 30), split)
    }

    /// The bar and stepper list render in the engine's bucket-line order.
    func testDriverOrderMatchesTheEngineBucketLine() {
        XCTAssertEqual(DriverSlug.allCases, [.codex, .claude, .grok, .agy])
    }

    func testAntigravityOnlyReceivesTrafficWhenExplicitlyRaised() {
        for driver in DriverSlug.allCases where driver != .agy {
            for target in 0...100 {
                XCTAssertEqual(DriverTrafficSplit.engineDefault.adjusting(driver, to: target).agy, 0)
            }
        }
        let start = DriverTrafficSplit(grok: 20, claude: 30, codex: 40, agy: 10)
        XCTAssertEqual(start.adjusting(.agy, to: 90), DriverTrafficSplit(grok: 10, claude: 0, codex: 0, agy: 90))
        XCTAssertEqual(start.adjusting(.agy, to: 0), DriverTrafficSplit(grok: 20, claude: 40, codex: 40, agy: 0))
        XCTAssertEqual(start.adjusting(.grok, to: 100), DriverTrafficSplit(grok: 100, claude: 0, codex: 0, agy: 0))
    }

    func testValidationIncludesFourthShareAndRejectsOutOfRangeShares() {
        XCTAssertTrue(DriverTrafficSplit(grok: 0, claude: 0, codex: 0, agy: 100).isValid)
        XCTAssertFalse(DriverTrafficSplit(grok: 0, claude: 100, codex: 0, agy: 1).isValid)
        XCTAssertFalse(DriverTrafficSplit(grok: 0, claude: 101, codex: 0, agy: -1).isValid)
    }
}
