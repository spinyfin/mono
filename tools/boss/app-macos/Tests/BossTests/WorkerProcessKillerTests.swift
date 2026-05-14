import Darwin
import XCTest
@testable import Boss

/// Coverage for the kill ladder the macOS app runs when the engine
/// asks it to release a worker pane. The tests use injected closures
/// so we don't depend on real syscalls / wall time — the assertions
/// are about *which* signals get sent in *what order*, with the
/// SIGKILL escalation gated on whether the process is still alive
/// after the grace window.
final class WorkerProcessKillerTests: XCTestCase {
    func testSendsSigtermToProcessGroupThenSkipsSigkillWhenProcessExits() async {
        // Happy path: SIGTERM lands, the worker exits cleanly inside
        // the grace window, no SIGKILL needed. Mirrors the
        // engine-side `signal_shell_pids` shape: don't escalate when
        // the process is already gone, so we don't churn an
        // already-released pid.
        let pid: pid_t = 4242
        let pgid: pid_t = 4200
        let recorder = SignalRecorder()

        await WorkerProcessKiller.killForegroundProcessTree(
            pid: pid,
            graceSeconds: 5,
            signal: recorder.signal,
            getpgid: { p in p == pid ? pgid : -1 },
            isAlive: { _ in false },
            sleep: { _ in }
        )

        let calls = recorder.calls
        XCTAssertEqual(calls.count, 1, "should only send SIGTERM when the process exits cleanly")
        XCTAssertEqual(calls.first?.pid, -pgid, "SIGTERM must target the process group (negated pgid)")
        XCTAssertEqual(calls.first?.signal, SIGTERM)
    }

    func testEscalatesToSigkillWhenProcessSurvivesGrace() async {
        // The bug the AI was filed to fix: a `claude` that traps
        // SIGTERM (node + handlers) stays alive past the grace
        // window. We must escalate to SIGKILL on the same process
        // group so the worker really dies. Asserting the *order*
        // of the calls — SIGTERM first, SIGKILL after the grace —
        // is what proves the ladder is wired up the right way round.
        let pid: pid_t = 8888
        let pgid: pid_t = 8800
        let recorder = SignalRecorder()

        await WorkerProcessKiller.killForegroundProcessTree(
            pid: pid,
            graceSeconds: 5,
            signal: recorder.signal,
            getpgid: { _ in pgid },
            isAlive: { _ in true },
            sleep: { _ in }
        )

        let calls = recorder.calls
        XCTAssertEqual(calls.count, 2, "stubborn process should receive SIGTERM then SIGKILL")
        XCTAssertEqual(calls[0], SignalCall(pid: -pgid, signal: SIGTERM))
        XCTAssertEqual(calls[1], SignalCall(pid: -pgid, signal: SIGKILL))
    }

    func testFallsBackToPidWhenGetpgidFails() async {
        // `getpgid` returns -1 with errno=ESRCH when the process is
        // already gone. Our fallback should signal the bare pid (a
        // best-effort attempt; `kill` will likely return ESRCH too)
        // rather than calling `kill(-(-1), …)` which would
        // catastrophically broadcast a signal to every process in the
        // caller's session. This test is the guard against the "kill
        // everything" regression.
        let pid: pid_t = 1234
        let recorder = SignalRecorder()

        await WorkerProcessKiller.killForegroundProcessTree(
            pid: pid,
            graceSeconds: 0,
            signal: recorder.signal,
            getpgid: { _ in -1 },
            isAlive: { _ in false },
            sleep: { _ in }
        )

        let calls = recorder.calls
        XCTAssertEqual(calls.count, 1)
        XCTAssertEqual(calls.first?.pid, pid, "must signal the lead pid, not negative-1")
        XCTAssertEqual(calls.first?.signal, SIGTERM)
    }

    func testNoOpForNonPositivePid() async {
        // The pid we read from `ghostty_surface_foreground_pid` is 0
        // when libghostty hasn't spawned a child yet (surface init
        // pre-attach). The release path passes that through unchecked,
        // so the helper itself must refuse to signal pid<=0 — otherwise
        // we'd `kill(0, …)` which broadcasts to the whole process
        // group of the *caller* (the Boss app), which would be very
        // bad on macOS.
        let recorder = SignalRecorder()

        await WorkerProcessKiller.killForegroundProcessTree(
            pid: 0,
            graceSeconds: 1,
            signal: recorder.signal,
            getpgid: { _ in 0 },
            isAlive: { _ in false },
            sleep: { _ in XCTFail("must not sleep when pid is non-positive") }
        )

        XCTAssertTrue(recorder.calls.isEmpty, "no-op for pid <= 0; got \(recorder.calls)")
    }

    func testSignalTargetPrefersNegatedPgid() {
        // Direct coverage of the helper used to derive the kill(2)
        // pid argument. Group signalling is the whole reason this
        // helper exists — a regression to bare-pid signalling would
        // let `claude`'s children survive.
        let resolved = WorkerProcessKiller.signalTarget(
            pid: 100,
            getpgid: { _ in 50 }
        )
        XCTAssertEqual(resolved, -50)
    }

    func testSignalTargetFallsBackToPidOnPgidFailure() {
        let resolved = WorkerProcessKiller.signalTarget(
            pid: 100,
            getpgid: { _ in -1 }
        )
        XCTAssertEqual(resolved, 100)
    }
}

private struct SignalCall: Equatable {
    let pid: pid_t
    let signal: Int32
}

private final class SignalRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var storage: [SignalCall] = []

    var calls: [SignalCall] {
        lock.lock()
        defer { lock.unlock() }
        return storage
    }

    /// `Signaler` shape: records the call and reports success. Wrapped
    /// in a `@Sendable` closure so the helper's task-detached usage
    /// stays warning-free under strict concurrency.
    var signal: @Sendable (pid_t, Int32) -> Int32 {
        { [self] pid, sig in
            self.lock.lock()
            self.storage.append(SignalCall(pid: pid, signal: sig))
            self.lock.unlock()
            return 0
        }
    }
}
