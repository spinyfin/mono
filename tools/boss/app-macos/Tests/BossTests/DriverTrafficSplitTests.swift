import XCTest
@testable import Boss

/// Covers the Settings driver-traffic control's editing model. The engine
/// rejects any split whose shares do not sum to exactly 100 — it never
/// repairs one — so the UI's contract is that `adjusting(_:to:)` can only
/// ever produce a valid split, and that every valid split is reachable
/// through it without passing through a rejected intermediate.
final class DriverTrafficSplitTests: XCTestCase {
    /// The shipped starting point: everything on the engine's default
    /// driver, nothing on Grok.
    func testEngineDefaultIsAllClaude() {
        let split = DriverTrafficSplit.engineDefault
        XCTAssertEqual(split.grok, 0)
        XCTAssertEqual(split.claude, 100)
        XCTAssertEqual(split.codex, 0)
        XCTAssertTrue(split.isValid)
    }

    /// Raising a share drains Claude first — the engine's default driver
    /// holds the traffic nobody has deliberately claimed.
    func testRaisingAShareTakesFromClaudeFirst() {
        let split = DriverTrafficSplit.engineDefault.adjusting(.grok, to: 20)
        XCTAssertEqual(split, DriverTrafficSplit(grok: 20, claude: 80, codex: 0))
    }

    /// Once Claude is exhausted the next donor gives way, and the result is
    /// still exactly 100.
    func testRaisingPastClaudeDrainsTheNextDonor() {
        let start = DriverTrafficSplit(grok: 20, claude: 30, codex: 50)
        let split = start.adjusting(.grok, to: 90)
        XCTAssertEqual(split, DriverTrafficSplit(grok: 90, claude: 0, codex: 10))
    }

    /// Lowering a share hands the whole surplus to the first donor.
    func testLoweringAShareGivesBackToTheFirstDonor() {
        let start = DriverTrafficSplit(grok: 40, claude: 30, codex: 30)
        XCTAssertEqual(start.adjusting(.grok, to: 10), DriverTrafficSplit(grok: 10, claude: 60, codex: 30))
        // Lowering Claude itself has to go somewhere: Codex before Grok, so
        // Grok only ever receives traffic the operator explicitly gave it.
        XCTAssertEqual(start.adjusting(.claude, to: 0), DriverTrafficSplit(grok: 40, claude: 0, codex: 60))
    }

    /// A driver can be taken to 100, zeroing the other two in one edit.
    func testRaisingToOneHundredZeroesTheOthers() {
        for driver in DriverSlug.allCases {
            let split = DriverTrafficSplit(grok: 33, claude: 33, codex: 34).adjusting(driver, to: 100)
            XCTAssertEqual(split.share(for: driver), 100, "\(driver)")
            XCTAssertTrue(split.isValid, "\(driver)")
            for other in DriverSlug.allCases where other != driver {
                XCTAssertEqual(split.share(for: other), 0, "\(other) under \(driver) at 100")
            }
        }
    }

    /// Every valid split is reachable, and no step along the way is invalid.
    /// Two single-share edits are enough for any target: set one share, then
    /// a second, and the third takes the remainder.
    func testEveryValidSplitIsReachableWithoutAnInvalidIntermediate() {
        for grok in stride(from: 0, through: 100, by: 5) {
            for claude in stride(from: 0, through: 100 - grok, by: 5) {
                let codex = 100 - grok - claude
                let first = DriverTrafficSplit.engineDefault.adjusting(.grok, to: grok)
                XCTAssertTrue(first.isValid, "intermediate \(first) is invalid")
                let second = first.adjusting(.codex, to: codex)
                XCTAssertTrue(second.isValid, "intermediate \(second) is invalid")
                XCTAssertEqual(
                    second,
                    DriverTrafficSplit(grok: grok, claude: claude, codex: codex),
                    "target grok=\(grok) claude=\(claude) codex=\(codex) unreachable"
                )
            }
        }
    }

    /// Out-of-range input from a stepper is clamped into range and still
    /// yields a valid split — the sum invariant never depends on the caller.
    func testOutOfRangeValuesAreClampedAndStillSumToOneHundred() {
        let low = DriverTrafficSplit(grok: 40, claude: 30, codex: 30).adjusting(.grok, to: -25)
        XCTAssertEqual(low, DriverTrafficSplit(grok: 0, claude: 70, codex: 30))
        let high = DriverTrafficSplit(grok: 40, claude: 30, codex: 30).adjusting(.grok, to: 250)
        XCTAssertEqual(high, DriverTrafficSplit(grok: 100, claude: 0, codex: 0))
    }

    /// Setting a share to what it already is is a no-op, so the view model's
    /// change guard suppresses a pointless round trip.
    func testAdjustingToTheCurrentValueIsANoOp() {
        let split = DriverTrafficSplit(grok: 40, claude: 30, codex: 30)
        XCTAssertEqual(split.adjusting(.claude, to: 30), split)
    }

    /// The bar and stepper list render in the engine's bucket-line order.
    func testDriverOrderMatchesTheEngineBucketLine() {
        XCTAssertEqual(DriverSlug.allCases, [.codex, .claude, .grok])
    }
}
