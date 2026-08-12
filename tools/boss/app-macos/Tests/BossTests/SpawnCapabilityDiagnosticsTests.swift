import AppKit
import XCTest
@testable import Boss

/// Covers the 2026-07-30 "spawn fails and burns work items when the screen
/// is locked" defect's app half: the measured host-capability verdict that
/// replaces the old hardcoded guess, the NACK message contract built from
/// it, and the live-work-scoped display assertion.
///
/// The measurement these tests encode, taken on a real host:
///
/// ```text
/// CVDisplayLinkCreateWithCGDisplays(count: 0) -> -6661 kCVReturnInvalidArgument, link NULL
/// CVDisplayLinkCreateWithCGDisplays(bogus id) -> -6670 kCVReturnInvalidDisplay,  link NULL
/// CVDisplayLinkCreateWithCGDisplays(main)     ->     0 kCVReturnSuccess,         link non-null
/// ```
///
/// i.e. zero active displays makes surface creation impossible, which is
/// why `activeDisplayCount` — and nothing else on the snapshot — is the
/// predicate.
final class SpawnCapabilityDiagnosticsTests: XCTestCase {

    private func snapshot(
        active: Int,
        online: Int = 1,
        asleep: Bool = false,
        locked: Bool = false,
        onConsole: Bool = true,
        screens: Int = 1
    ) -> HostDisplaySnapshot {
        HostDisplaySnapshot.make(
            activeDisplayCount: active,
            onlineDisplayCount: online,
            mainDisplayAsleep: asleep,
            sessionLocked: locked,
            sessionOnConsole: onConsole,
            screenCount: screens
        )
    }

    // MARK: - The verdict

    func testZeroActiveDisplaysBlocksSpawn() {
        let verdict = SpawnCapability.verdict(for: snapshot(active: 0, online: 1))
        XCTAssertTrue(verdict.isBlocked)
    }

    func testAnyActiveDisplayAllowsSpawn() {
        XCTAssertEqual(SpawnCapability.verdict(for: snapshot(active: 1)), .canHostPane)
        XCTAssertEqual(SpawnCapability.verdict(for: snapshot(active: 2, online: 2)), .canHostPane)
    }

    /// The regression that started the whole investigation: the old code
    /// asserted "no active display" from a *lock*. A locked session with a
    /// live display can host a pane perfectly well, so lock state must not
    /// enter the predicate — only the measured active-display count does.
    func testLockedSessionWithAnActiveDisplayIsNotBlocked() {
        let verdict = SpawnCapability.verdict(for: snapshot(active: 1, locked: true))
        XCTAssertEqual(
            verdict,
            .canHostPane,
            "a screen lock alone must never be treated as 'cannot spawn' — only a zero active-display count is"
        )
    }

    /// The converse: an asleep main display that has nonetheless left the
    /// active list non-empty is also not blocked. Only the count decides.
    func testAsleepMainDisplayWithNonZeroActiveCountIsNotBlocked() {
        XCTAssertEqual(SpawnCapability.verdict(for: snapshot(active: 1, asleep: true)), .canHostPane)
    }

    func testBlockedReasonNamesTheCauseAndWhatClearsIt() {
        guard case .environmentUnavailable(let reason) = SpawnCapability.verdict(for: snapshot(active: 0)) else {
            return XCTFail("expected a blocked verdict")
        }
        // Rendered verbatim in the operator's dispatch-paused banner, so it
        // has to answer both "what happened" and "what clears it".
        XCTAssertTrue(reason.contains("no active display"), reason)
        XCTAssertTrue(reason.contains("Clears automatically"), reason)
    }

    // MARK: - The snapshot is evidence, not a guess

    func testSummaryCarriesEveryMeasuredField() {
        let summary = snapshot(active: 0, online: 2, asleep: true, locked: true, onConsole: false, screens: 3).summary
        for expected in [
            "active_displays=0",
            "online_displays=2",
            "main_display_asleep=true",
            "session_locked=true",
            "session_on_console=false",
            "ns_screens=3",
        ] {
            XCTAssertTrue(summary.contains(expected), "summary missing \(expected): \(summary)")
        }
    }

    /// The live reader must agree with itself: whatever this host is, the
    /// verdict derived from a fresh snapshot equals `current()`.
    @MainActor
    func testCurrentMatchesVerdictOfSnapshot() {
        XCTAssertEqual(SpawnCapability.current(), SpawnCapability.verdict(for: SpawnCapability.snapshot()))
    }

    // MARK: - The NACK message contract

    func testEnvironmentalNackReasonCarriesTheMeasuredState() {
        let host = snapshot(active: 0, online: 1, asleep: true, locked: true)
        let reason = GhosttyTerminalHostView.surfaceFailureNackReason(
            host: host,
            verdict: SpawnCapability.verdict(for: host)
        )
        XCTAssertTrue(reason.contains("ghostty_surface_new returned NULL"), reason)
        XCTAssertTrue(reason.contains("no active display"), reason)
        XCTAssertTrue(reason.contains("active_displays=0"), reason)
    }

    /// A NULL surface *with* a usable display is a genuine failure whose
    /// cause we do not know. The message must say so rather than blaming the
    /// display — blaming the display when the display was fine is precisely
    /// what misdirected the 2026-07-30 investigation.
    func testUnclassifiedNackReasonDoesNotBlameTheDisplay() {
        let host = snapshot(active: 2, online: 2, screens: 2)
        let reason = GhosttyTerminalHostView.surfaceFailureNackReason(
            host: host,
            verdict: SpawnCapability.verdict(for: host)
        )
        XCTAssertTrue(reason.contains("NOT the host display state"), reason)
        XCTAssertFalse(reason.lowercased().contains("sleep/wake"), reason)
        XCTAssertTrue(reason.contains("active_displays=2"), reason)
    }

    // MARK: - The live-work display assertion

    /// The direct answer to "we put a lock in — did that not work?": the
    /// process-lifetime token opts out of App Nap and explicitly permits
    /// display sleep. This one is the display-sleep suppressor, and it must
    /// carry `.idleDisplaySleepDisabled` or it repeats the same mistake.
    @MainActor
    func testAssertionOptionsActuallySuppressDisplaySleep() {
        XCTAssertTrue(
            LiveWorkDisplayAssertion.activityOptions.contains(.idleDisplaySleepDisabled),
            "the whole point of this token is display-sleep suppression"
        )
        XCTAssertTrue(LiveWorkDisplayAssertion.activityOptions.contains(.userInitiated))
    }

    @MainActor
    func testAssertionIsHeldOnlyWhileWorkersAreLive() {
        var begun: [ProcessInfo.ActivityOptions] = []
        var ended = 0
        let assertion = LiveWorkDisplayAssertion(
            begin: { options, _ in
                begun.append(options)
                return NSString(string: "token") as NSObjectProtocol
            },
            end: { _ in ended += 1 }
        )

        XCTAssertFalse(assertion.isHeld, "an idle Boss must assert nothing")

        assertion.update(liveWorkerCount: 1)
        XCTAssertTrue(assertion.isHeld)
        XCTAssertEqual(begun.count, 1)
        XCTAssertEqual(begun.first, LiveWorkDisplayAssertion.activityOptions)

        // Idempotent while work continues — no stacked tokens.
        assertion.update(liveWorkerCount: 3)
        assertion.update(liveWorkerCount: 2)
        XCTAssertEqual(begun.count, 1, "assertion must not be re-acquired while already held")
        XCTAssertEqual(ended, 0)

        // Released when the fleet drains — this is what keeps it from being
        // the user-hostile "display never sleeps while Boss runs" version.
        assertion.update(liveWorkerCount: 0)
        XCTAssertFalse(assertion.isHeld)
        XCTAssertEqual(ended, 1)

        // And re-acquirable for the next batch.
        assertion.update(liveWorkerCount: 1)
        XCTAssertTrue(assertion.isHeld)
        XCTAssertEqual(begun.count, 2)
    }

    @MainActor
    func testReleasingIsIdempotentWhenNothingIsHeld() {
        var ended = 0
        let assertion = LiveWorkDisplayAssertion(
            begin: { _, _ in NSString(string: "token") as NSObjectProtocol },
            end: { _ in ended += 1 }
        )
        assertion.update(liveWorkerCount: 0)
        assertion.update(liveWorkerCount: 0)
        XCTAssertEqual(ended, 0, "endActivity must never be called for a token that was never begun")
    }
}
