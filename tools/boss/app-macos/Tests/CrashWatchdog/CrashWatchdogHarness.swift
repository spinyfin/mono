import CrashWatchdog
import Darwin

// Fault injection harness for the crash-handler livelock.
//
// Each mode installs a stand-in for Sentry/Breakpad's SIGABRT handler, then
// aborts. `crash_watchdog_test.sh` drives the modes and asserts what the
// process does next. The point is to reproduce the 2026-07-29 failure mode
// deliberately — a crash handler that never returns — rather than to assert
// on the shape of our own code.
//
// Modes:
//   spin-guarded    livelocking prior handler + CrashWatchdog installed.
//                   Must die by SIGALRM within the grace period.
//   spin-unguarded  the same livelocking prior handler, no watchdog. Must
//                   hang, which is what makes `spin-guarded` meaningful.
//   chain-returning prior handler that returns normally + CrashWatchdog.
//                   Must die promptly by SIGABRT, not by the watchdog: the
//                   bound is a backstop, not the normal termination path.

/// Thread id the fake guard claims ownership by. Recorded on the main thread;
/// the abort happens on a secondary thread, so the spinning thread never
/// matches — exactly the state the wedged process was sampled in.
nonisolated(unsafe) private var guardOwnerThread = pthread_self()

/// Marker bytes, allocated before any handler can run so the signal handlers
/// only ever `write(2)` and never allocate.
nonisolated(unsafe) private var priorHandlerMarker: UnsafeRawPointer?
nonisolated(unsafe) private var priorHandlerMarkerLength = 0

private func emitFromHandler() {
    if let marker = priorHandlerMarker, priorHandlerMarkerLength > 0 {
        _ = write(STDOUT_FILENO, marker, priorHandlerMarkerLength)
    }
}

private func emit(_ text: String) {
    var bytes = Array(text.utf8)
    _ = bytes.withUnsafeMutableBufferPointer { buffer in
        write(STDOUT_FILENO, buffer.baseAddress, buffer.count)
    }
}

/// Stand-in for `sentry__breakpad_backend_callback` wedged inside
/// `sentry__block_for_signal_handler`. sentry-native 0.7.8 spins here with no
/// timeout and no bail-out whenever `g_in_signal_handler` is latched by a
/// thread other than the current one:
///
///     while (__sync_fetch_and_add(&g_in_signal_handler, 0)) {
///         if (sentry__threadid_equal(sentry__current_thread(),
///                                    g_signal_handling_thread)) return false;
///         sentry__cpu_relax();
///     }
///
/// The `pthread_equal` call is kept because it is both the real upstream
/// escape test and an opaque call the optimiser cannot hoist the loop past.
private func livelockingPriorHandler(
    _ signo: Int32,
    _ info: UnsafeMutablePointer<siginfo_t>?,
    _ context: UnsafeMutableRawPointer?
) {
    emitFromHandler()
    while pthread_equal(pthread_self(), guardOwnerThread) == 0 {
        // Busy-wait on a guard nobody will ever release.
    }
}

/// Prior handler that behaves itself: does its work and returns. Breakpad
/// normally `_exit`s instead, so returning is the stricter case — it proves
/// the chain re-raises rather than letting execution fall through.
private func returningPriorHandler(
    _ signo: Int32,
    _ info: UnsafeMutablePointer<siginfo_t>?,
    _ context: UnsafeMutableRawPointer?
) {
    emitFromHandler()
}

private typealias SigactionCall = @convention(c) (
    Int32, UnsafePointer<sigaction>?, UnsafeMutablePointer<sigaction>?
) -> Int32

@main
enum CrashWatchdogHarness {
    static func main() {
        guardOwnerThread = pthread_self()
        allocateMarker("harness: prior-handler-entered\n")

        let mode = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : ""
        switch mode {
        case "spin-guarded":
            installPriorHandler(livelockingPriorHandler)
            let hooked = CrashWatchdog.install()
            emit("harness: watchdog hooked \(hooked.count) signal(s)\n")
            abortOnSecondaryThread()
        case "spin-unguarded":
            installPriorHandler(livelockingPriorHandler)
            emit("harness: watchdog not installed\n")
            abortOnSecondaryThread()
        case "chain-returning":
            installPriorHandler(returningPriorHandler)
            CrashWatchdog.install()
            emit("harness: watchdog installed\n")
            abortOnSecondaryThread()
        default:
            emit("harness: unknown mode '\(mode)'\n")
            exit(64)
        }
    }

    private static func allocateMarker(_ text: String) {
        let bytes = Array(text.utf8)
        let buffer = UnsafeMutablePointer<UInt8>.allocate(capacity: bytes.count)
        buffer.update(from: bytes, count: bytes.count)
        priorHandlerMarker = UnsafeRawPointer(buffer)
        priorHandlerMarkerLength = bytes.count
    }

    private static func installPriorHandler(
        _ handler: @escaping @convention(c) (
            Int32, UnsafeMutablePointer<siginfo_t>?, UnsafeMutableRawPointer?
        ) -> Void
    ) {
        let sigactionCall: SigactionCall = sigaction
        var action = sigaction()
        action.__sigaction_u.__sa_sigaction = handler
        action.sa_flags = SA_SIGINFO
        sigemptyset(&action.sa_mask)
        _ = sigactionCall(SIGABRT, &action, nil)
    }

    /// Abort off the main thread, so the spinning handler and the fake guard
    /// owner really are different threads. The main thread then parks in
    /// `pthread_join` — a plain kernel wait, so nothing here competes with
    /// the watchdog's `alarm()` the way `sleep(3)` could.
    private static func abortOnSecondaryThread() -> Never {
        var thread: pthread_t?
        let created = pthread_create(&thread, nil, { _ in abort() }, nil)
        if created != 0 {
            emit("harness: pthread_create failed\n")
            exit(64)
        }
        if let thread {
            pthread_join(thread, nil)
        }
        emit("harness: aborting thread returned unexpectedly\n")
        exit(65)
    }
}
