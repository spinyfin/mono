import XCTest

@testable import CrashWatchdog

/// Unit coverage for the parts of [[CrashWatchdog]] that can be exercised
/// without aborting a process. The behaviour that matters — "an abort always
/// terminates the process within a bounded time" — is covered end-to-end by
/// `crash_watchdog_test.sh`, which drives a real crashing subprocess.
final class CrashWatchdogTests: XCTestCase {
    func testGraceSecondsDefaultsWhenUnset() {
        XCTAssertEqual(
            CrashWatchdog.graceSeconds(fromEnvironmentValue: nil),
            CrashWatchdog.defaultGraceSeconds
        )
    }

    func testGraceSecondsDefaultsOnEmptyOrUnparseableValues() {
        for raw in ["", "   ", "abc", "-5", "3.5", "2s"] {
            XCTAssertEqual(
                CrashWatchdog.graceSeconds(fromEnvironmentValue: raw),
                CrashWatchdog.defaultGraceSeconds,
                "expected \"\(raw)\" to fall back to the default"
            )
        }
    }

    func testGraceSecondsAcceptsAndTrimsValidValues() {
        XCTAssertEqual(CrashWatchdog.graceSeconds(fromEnvironmentValue: "5"), 5)
        XCTAssertEqual(CrashWatchdog.graceSeconds(fromEnvironmentValue: " 30\n"), 30)
    }

    /// Zero would cancel the `alarm()` rather than shorten it — i.e. it would
    /// silently disable the very bound this type exists to provide.
    func testGraceSecondsClampsZeroUpToTheMinimum() {
        XCTAssertEqual(
            CrashWatchdog.graceSeconds(fromEnvironmentValue: "0"),
            CrashWatchdog.minimumGraceSeconds
        )
    }

    func testGraceSecondsClampsAbsurdlyLargeValuesDown() {
        XCTAssertEqual(
            CrashWatchdog.graceSeconds(fromEnvironmentValue: "999999"),
            CrashWatchdog.maximumGraceSeconds
        )
    }

    /// `install(graceSeconds:)` routes its explicit override through the same
    /// clamp as the environment variable, so a programmatic 0 cannot disable
    /// the watchdog either.
    func testClampAppliesTheSameBoundsToExplicitOverrides() {
        XCTAssertEqual(CrashWatchdog.clampGraceSeconds(0), CrashWatchdog.minimumGraceSeconds)
        XCTAssertEqual(
            CrashWatchdog.clampGraceSeconds(.max), CrashWatchdog.maximumGraceSeconds
        )
        XCTAssertEqual(CrashWatchdog.clampGraceSeconds(7), 7)
    }

    /// Darwin packs `SIG_DFL` (0), `SIG_IGN` (1), `SIG_HOLD` (5) and
    /// `SIG_ERR` (-1) into the same field as a real function pointer. Calling
    /// one of those as a function would jump to a null-ish address from inside
    /// a signal handler.
    func testReservedDispositionsAreNotChainable() {
        XCTAssertFalse(CrashWatchdog.isChainableHandler(0))
        XCTAssertFalse(CrashWatchdog.isChainableHandler(1))
        XCTAssertFalse(CrashWatchdog.isChainableHandler(5))
        XCTAssertFalse(CrashWatchdog.isChainableHandler(UInt.max))
    }

    func testRealFunctionPointersAreChainable() {
        let handler: @convention(c) (Int32) -> Void = { _ in }
        let bits = UInt(bitPattern: unsafeBitCast(handler, to: Int.self))
        XCTAssertTrue(CrashWatchdog.isChainableHandler(bits))
    }

    /// SIGABRT is the signal the 2026-07-29 livelock arrived on, and the only
    /// one Breakpad handles via a POSIX handler on macOS. SIGTRAP must stay
    /// out: a debugger breakpoint is not a crash.
    func testWatchedSignalsCoverAbortAndExcludeTrap() {
        XCTAssertTrue(CrashWatchdog.watchedSignals.contains(SIGABRT))
        XCTAssertFalse(CrashWatchdog.watchedSignals.contains(SIGTRAP))
    }

    func testTimeoutExitCodeMirrorsSigalrmTermination() {
        XCTAssertEqual(CrashWatchdog.timeoutExitCode, 128 + SIGALRM)
    }
}
