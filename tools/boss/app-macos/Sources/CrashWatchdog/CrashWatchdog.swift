import Darwin

/// A bounded-time termination guarantee for fatal signals.
///
/// ## Why this exists
///
/// On 2026-07-29 Boss did not crash when it aborted — it livelocked. The
/// crash-handler thread spun at 100 % CPU for ~11.5 hours, the main runloop
/// starved, and the engine piled up timed-out RPCs against a zombied app.
///
/// The spin is upstream. Boss statically links `sentry-native 0.7.8` through
/// the prebuilt `GhosttyKit.xcframework` (`ghostty` → `pkg/sentry` →
/// sentry-native), and `sentry_sync.c` in that version guards all crash-time
/// work with:
///
/// ```c
/// bool sentry__block_for_signal_handler(void) {
///     while (__sync_fetch_and_add(&g_in_signal_handler, 0)) {
///         if (sentry__threadid_equal(sentry__current_thread(),
///                                    g_signal_handling_thread)) {
///             return false;
///         }
///         sentry__cpu_relax();   // <- unbounded busy-spin
///     }
///     return true;
/// }
/// ```
///
/// That loop has no timeout and no give-up path. `g_in_signal_handler` is
/// cleared only by `sentry__leave_signal_handler()`, which is the *last*
/// statement of `sentry__breakpad_backend_callback` — after the minidump and
/// envelope have been written to disk. Any crash-handling path that cannot
/// run to completion therefore latches the flag at 1 permanently, and every
/// later entrant (the guard is also reached from `sentry__mutex_lock` /
/// `sentry__mutex_unlock` / `sentry__cond_wait`) spins forever.
///
/// We cannot fix that from here: it is upstream C compiled into a binary-only
/// xcframework, and even current sentry-native `master` still spins
/// unboundedly for a thread that does not own the guard. So Boss enforces the
/// missing guarantee itself.
///
/// ## How it works
///
/// `install()` chains a handler *in front of* whatever Sentry/Breakpad already
/// installed. On a fatal signal that handler:
///
/// 1. arms a kernel `alarm()` deadline, then
/// 2. calls the previously installed handler (so crash reporting still runs
///    exactly as before), and
/// 3. if that handler ever returns, restores the default disposition and
///    re-raises, so the process dies with the original signal.
///
/// The deadline is deliberately a **kernel timer plus a default signal
/// disposition**, not a watchdog thread. A watchdog thread would need to be
/// scheduled, and could itself be blocked behind whatever wedged the crash
/// handler (a held malloc lock, a spinning peer, a starved runloop). `alarm()`
/// needs nothing from userspace: once armed, the kernel terminates the process
/// whether or not a single line of our code ever runs again.
///
/// Every call the signal handlers make — `sigaction`, `alarm`, `signal`,
/// `sigprocmask`, `write`, `raise`, `_exit` — is on POSIX's
/// async-signal-safe list. All state the handlers touch is allocated during
/// `install()` and reached through raw pointers, so no Swift global
/// initializer, allocation, or exclusivity check runs on the crash path.
///
/// ## What this does not cover
///
/// Only signal-delivered faults. On macOS, Breakpad claims hardware faults
/// (`EXC_BAD_ACCESS`, `EXC_BREAKPOINT`, …) through a Mach exception port and
/// handles them on its own thread, so no POSIX signal is generated and this
/// chain never sees them. `abort()` — the reported failure mode, and the
/// terminal path of every uncaught `NSException` and every Swift runtime trap
/// that lowers to `abort` — does deliver `SIGABRT` to the faulting thread and
/// is fully covered.
public enum CrashWatchdog {
    /// Seconds the crash handler gets before the process is terminated
    /// regardless. Generous on purpose: writing a Breakpad minidump plus a
    /// Sentry envelope for a multi-GB-RSS process is well under a second in
    /// practice, so a bound this loose only ever fires on a genuine wedge.
    public static let defaultGraceSeconds: UInt32 = 10

    /// Lower bound for the configured grace. `alarm()` has one-second
    /// granularity, and 0 would *cancel* the timer rather than shorten it.
    public static let minimumGraceSeconds: UInt32 = 1

    /// Upper bound for the configured grace. Anything past this is a
    /// misconfiguration that would recreate the incident we are fixing.
    public static let maximumGraceSeconds: UInt32 = 600

    /// Operator override for the grace period, in whole seconds. Read once,
    /// during `install()`.
    public static let graceEnvironmentVariable = "BOSS_CRASH_WATCHDOG_SECONDS"

    /// Exit status used only if the process somehow survives `raise(SIGALRM)`
    /// with the default disposition restored — i.e. never, in practice.
    /// 128 + `SIGALRM` mirrors the shell's "killed by signal 14" convention,
    /// so the two termination shapes read the same in logs.
    public static let timeoutExitCode: Int32 = 128 + SIGALRM

    /// Fatal signals whose handler chain we bound.
    ///
    /// `SIGABRT` is the one that matters on macOS: Breakpad installs a POSIX
    /// handler for it specifically (`abort()` produces no Mach exception), and
    /// it is the signal the 2026-07-29 livelock came in on. The rest are
    /// cheap defence in depth — they cost one `sigaction` each, they cover the
    /// SwiftPM dev build and any future backend that does use POSIX handlers
    /// for faults, and hooking them cannot make a fatal signal less fatal.
    ///
    /// `SIGTRAP` is deliberately absent: debuggers rely on it, and a
    /// breakpoint is not a crash.
    public static let watchedSignals: [Int32] = [
        SIGABRT, SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGSYS,
    ]

    /// Resolve the grace period from a raw environment string.
    ///
    /// Pure so the clamping contract is directly testable: unset, empty, and
    /// unparseable values fall back to `defaultGraceSeconds`; parseable values
    /// are clamped into `[minimumGraceSeconds, maximumGraceSeconds]` so a typo
    /// can neither disable the bound (0) nor push it past usefulness.
    public static func graceSeconds(fromEnvironmentValue raw: String?) -> UInt32 {
        guard let raw else { return defaultGraceSeconds }
        let trimmed = raw.trimmingASCIIWhitespace()
        guard !trimmed.isEmpty, let parsed = UInt32(trimmed) else {
            return defaultGraceSeconds
        }
        return clampGraceSeconds(parsed)
    }

    /// Clamp a requested grace into the usable range. `0` in particular must
    /// never reach `alarm()`, where it cancels the timer instead of shortening
    /// it — i.e. silently disables the guarantee this type exists to make.
    public static func clampGraceSeconds(_ seconds: UInt32) -> UInt32 {
        min(max(seconds, minimumGraceSeconds), maximumGraceSeconds)
    }

    /// Whether a previous `sa_handler` disposition is a real function we can
    /// forward to.
    ///
    /// Darwin encodes the reserved dispositions as small integers in the same
    /// field: `SIG_DFL` 0, `SIG_IGN` 1, `SIG_HOLD` 5, `SIG_ERR` -1. Calling
    /// any of them as a function pointer would jump to a null-ish address, so
    /// they are filtered out here rather than at the (signal-handler) call
    /// site. Pure, so the filter is testable without installing anything.
    public static func isChainableHandler(_ bitPattern: UInt) -> Bool {
        bitPattern > 5 && bitPattern != UInt.max
    }

    /// Install the chained handlers. Idempotent — later calls are no-ops and
    /// return an empty array, so a second entry point cannot displace the
    /// saved Sentry/Breakpad dispositions with our own.
    ///
    /// Call this *after* the crash reporter has installed its handlers
    /// (for Boss: immediately after `ghostty_init`, which is what initialises
    /// Sentry). Installing earlier would leave us behind Breakpad in the
    /// chain, where an unbounded spin never reaches us.
    ///
    /// - Parameter graceSeconds: overrides both the default and the
    ///   environment variable. Clamped like the environment value.
    /// - Returns: the signals actually hooked, for the caller to log.
    @discardableResult
    public static func install(graceSeconds override: UInt32? = nil) -> [Int32] {
        guard !crashWatchdogInstalled else { return [] }
        crashWatchdogInstalled = true

        let grace: UInt32
        if let override {
            grace = clampGraceSeconds(override)
        } else {
            grace = graceSeconds(fromEnvironmentValue: environmentValue(graceEnvironmentVariable))
        }
        crashWatchdogGraceSeconds = grace

        allocateHandlerState(graceSeconds: grace)

        var hooked: [Int32] = []
        guard let previousActions = crashWatchdogPreviousActions,
              let installAction = crashWatchdogSigaction else { return hooked }

        var chained = sigaction()
        chained.__sigaction_u.__sa_sigaction = crashWatchdogFatalSignalHandler
        chained.sa_flags = SA_SIGINFO
        sigemptyset(&chained.sa_mask)

        for signo in watchedSignals where signo > 0 && signo < crashWatchdogSignalTableCount {
            var current = sigaction()
            guard installAction(signo, nil, &current) == 0 else { continue }

            // Preserve an explicit "ignore this signal" disposition. Replacing
            // it would turn a signal the process deliberately discards into a
            // termination, which is a behaviour change, not a safety net.
            let currentHandlerBits = UInt(bitPattern: unsafeBitCast(
                current.__sigaction_u.__sa_handler, to: Int.self
            ))
            if current.sa_flags & SA_SIGINFO == 0, currentHandlerBits == 1 { continue }

            // Inherit SA_ONSTACK: if the existing handler asked to run on the
            // alternate signal stack, ours must too, or a stack-overflow
            // SIGSEGV would fault again the moment we are entered.
            var action = chained
            action.sa_flags |= (current.sa_flags & SA_ONSTACK)

            guard installAction(signo, &action, nil) == 0 else { continue }
            previousActions[Int(signo)] = current
            hooked.append(signo)
        }
        return hooked
    }

    private static func environmentValue(_ name: String) -> String? {
        guard let raw = getenv(name) else { return nil }
        return String(cString: raw)
    }

    /// Allocate every byte the signal handlers will touch, while we are still
    /// in ordinary (allocating, lock-taking) context.
    private static func allocateHandlerState(graceSeconds grace: UInt32) {
        // `sigaction` names both a struct and a C function in Swift's Darwin
        // overlay, and the struct wins at every call site. Binding the
        // function to an explicitly typed value disambiguates it once, and
        // stashing the pointer keeps the crash path free of the lazy global
        // initialiser a `let` at file scope would have needed.
        let sigactionCall: CrashWatchdogSigactionCall = sigaction
        crashWatchdogSigaction = sigactionCall

        let previousActions = UnsafeMutablePointer<sigaction>.allocate(
            capacity: Int(crashWatchdogSignalTableCount)
        )
        previousActions.initialize(
            repeating: sigaction(), count: Int(crashWatchdogSignalTableCount)
        )
        crashWatchdogPreviousActions = previousActions

        let armed = UnsafeMutablePointer<sig_atomic_t>.allocate(capacity: 1)
        armed.initialize(to: 0)
        crashWatchdogArmed = armed

        // Pre-built `struct sigaction`s so the handlers never build one on a
        // crashing stack. SA_NODEFER on the alarm action keeps SIGALRM
        // unblocked inside its own handler, so the final `raise(SIGALRM)`
        // takes effect immediately instead of merely going pending.
        let alarmAction = UnsafeMutablePointer<sigaction>.allocate(capacity: 1)
        alarmAction.initialize(to: sigaction())
        alarmAction.pointee.__sigaction_u.__sa_sigaction = crashWatchdogAlarmHandler
        alarmAction.pointee.sa_flags = SA_SIGINFO | SA_NODEFER
        sigemptyset(&alarmAction.pointee.sa_mask)
        crashWatchdogAlarmAction = alarmAction

        let defaultAlarmAction = UnsafeMutablePointer<sigaction>.allocate(capacity: 1)
        defaultAlarmAction.initialize(to: sigaction())
        defaultAlarmAction.pointee.__sigaction_u.__sa_handler = SIG_DFL
        defaultAlarmAction.pointee.sa_flags = 0
        sigemptyset(&defaultAlarmAction.pointee.sa_mask)
        crashWatchdogDefaultAlarmAction = defaultAlarmAction

        let text = "boss crash-watchdog: crash handler did not terminate the process within "
            + "\(grace)s of a fatal signal (see tools/boss/docs/crash-watchdog.md); "
            + "killing it now.\n"
        let bytes = Array(text.utf8)
        let message = UnsafeMutablePointer<UInt8>.allocate(capacity: bytes.count)
        message.update(from: bytes, count: bytes.count)
        crashWatchdogMessage = UnsafeRawPointer(message)
        crashWatchdogMessageLength = bytes.count
    }
}

extension String {
    fileprivate func trimmingASCIIWhitespace() -> String {
        let isSpace: (Character) -> Bool = { $0 == " " || $0 == "\t" || $0 == "\n" || $0 == "\r" }
        var view = Substring(self)
        while let first = view.first, isSpace(first) { view = view.dropFirst() }
        while let last = view.last, isSpace(last) { view = view.dropLast() }
        return String(view)
    }
}

// MARK: - Handler-reachable state
//
// Every global below is either written once during `install()` or is a raw
// pointer to storage allocated there. Each carries a *literal* initialiser, so
// the compiler emits it as statically initialised data rather than behind a
// lazy-initialiser (`swift_once`) accessor, and the handlers only ever read
// them — no `inout` access, so no exclusivity bookkeeping runs on the crash
// path either. Keep it that way: an initialiser that references another
// declaration would reintroduce a runtime call in a signal handler.

private let crashWatchdogSignalTableCount: Int32 = 64

/// Signature of POSIX `sigaction(2)`. See the note in `allocateHandlerState`
/// for why the function has to be reached through a typed value.
private typealias CrashWatchdogSigactionCall = @convention(c) (
    Int32, UnsafePointer<sigaction>?, UnsafeMutablePointer<sigaction>?
) -> Int32

nonisolated(unsafe) private var crashWatchdogSigaction: CrashWatchdogSigactionCall?
nonisolated(unsafe) private var crashWatchdogInstalled = false
/// Overwritten by `install()` with the resolved grace before any handler can
/// exist. The placeholder is the one-second floor rather than
/// `defaultGraceSeconds` so that a hypothetical read-before-install could only
/// ever make the watchdog fire *sooner* — never (as `0` would) cancel it.
nonisolated(unsafe) private var crashWatchdogGraceSeconds: UInt32 = 1
nonisolated(unsafe) private var crashWatchdogPreviousActions: UnsafeMutablePointer<sigaction>?
nonisolated(unsafe) private var crashWatchdogArmed: UnsafeMutablePointer<sig_atomic_t>?
nonisolated(unsafe) private var crashWatchdogAlarmAction: UnsafeMutablePointer<sigaction>?
nonisolated(unsafe) private var crashWatchdogDefaultAlarmAction: UnsafeMutablePointer<sigaction>?
nonisolated(unsafe) private var crashWatchdogMessage: UnsafeRawPointer?
nonisolated(unsafe) private var crashWatchdogMessageLength = 0

// MARK: - Signal handlers

/// Chained handler for every signal in `CrashWatchdog.watchedSignals`.
private func crashWatchdogFatalSignalHandler(
    _ signo: Int32,
    _ info: UnsafeMutablePointer<siginfo_t>?,
    _ context: UnsafeMutableRawPointer?
) {
    // Arm once. A crash that cascades into further fatal signals must not keep
    // pushing the deadline out — that is precisely how 11.5 hours of spinning
    // becomes indistinguishable from progress.
    if let armed = crashWatchdogArmed, armed.pointee == 0 {
        armed.pointee = 1
        if let alarmAction = crashWatchdogAlarmAction,
           let installAction = crashWatchdogSigaction {
            _ = installAction(SIGALRM, alarmAction, nil)
        }
        // Darwin's `abort(3)` blocks every signal except SIGABRT on the
        // calling thread before it raises, and SIGALRM is process-directed:
        // if this thread will not take it, some *other* thread has to, and
        // there is no guarantee one is in an interruptible state (a wedged
        // process is exactly where that assumption fails). Unblock it here so
        // the deadline lands on the thread we know is still running.
        var alarmOnly = sigset_t()
        sigemptyset(&alarmOnly)
        sigaddset(&alarmOnly, SIGALRM)
        pthread_sigmask(SIG_UNBLOCK, &alarmOnly, nil)

        alarm(crashWatchdogGraceSeconds)
    }

    // Hand off to Sentry/Breakpad exactly as if we were not here.
    if let previousActions = crashWatchdogPreviousActions,
       signo > 0, signo < crashWatchdogSignalTableCount {
        let previous = previousActions[Int(signo)]
        if previous.sa_flags & SA_SIGINFO != 0 {
            if let action = previous.__sigaction_u.__sa_sigaction {
                action(signo, info, context)
            }
        } else if let handler = previous.__sigaction_u.__sa_handler,
                  CrashWatchdog.isChainableHandler(
                      UInt(bitPattern: unsafeBitCast(handler, to: Int.self))
                  ) {
            handler(signo)
        }
    }

    // The previous handler returned instead of terminating (Breakpad normally
    // `_exit`s here). Restore the original disposition and re-raise so the
    // process dies now, with the signal it actually received.
    signal(signo, SIG_DFL)
    raise(signo)
}

/// Fires when the crash handler has blown its deadline. Runs in ordinary
/// signal-handler context — no allocation, no locks, no Swift runtime.
private func crashWatchdogAlarmHandler(
    _ signo: Int32,
    _ info: UnsafeMutablePointer<siginfo_t>?,
    _ context: UnsafeMutableRawPointer?
) {
    // Restore SIGALRM's default (terminate) disposition and re-arm a short
    // timer *before* touching stderr, so that even a write that blocks
    // forever — the disk was full when this incident happened — cannot keep
    // the process alive.
    if let defaultAlarmAction = crashWatchdogDefaultAlarmAction,
       let installAction = crashWatchdogSigaction {
        _ = installAction(SIGALRM, defaultAlarmAction, nil)
    }
    alarm(1)

    if let message = crashWatchdogMessage, crashWatchdogMessageLength > 0 {
        _ = write(STDERR_FILENO, message, crashWatchdogMessageLength)
    }

    // Die by signal rather than by exit code: `WIFSIGNALED` is what a
    // supervisor and macOS's own crash reporter both understand, and the
    // resulting report names the wedged thread.
    raise(SIGALRM)

    // Only reachable if SIGALRM is blocked despite SA_NODEFER.
    _exit(CrashWatchdog.timeoutExitCode)
}
