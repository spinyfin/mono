# Crash watchdog: why an abort now always kills Boss

If you are reading this because Boss printed

```
boss crash-watchdog: crash handler did not terminate the process within 10s of a
fatal signal (see tools/boss/docs/crash-watchdog.md); killing it now.
```

then Boss crashed, its crash reporter failed to finish handling that crash, and the watchdog terminated the process instead of letting it hang. The app will have died with signal 14 (`SIGALRM`, "Alarm clock"), and macOS will have written a crash report naming the wedged thread. **That report is the interesting artifact** — it shows what the crash handler was stuck on.

## The incident this exists for

On 2026-07-29 Boss v1.0.427 aborted and did not crash. It livelocked: the crash-handler thread spun at 100 % CPU for ~11.5 hours, the main runloop starved, and the engine piled up timed-out RPCs (`send_to_app: timed out waiting for app response`, queue depth 100+, oldest age ~15 minutes) against an app that was alive but useless. A clean crash would have cost minutes; the hang cost most of a day.

A `sample` of the wedged process put every one of 1619 samples on the crash-handler thread in `sentry__enter_signal_handler` → `pthread_equal` → `pthread_self`, reached via `TerminalLoopLog.writeLine` → `-[NSFileHandle writeData:]` → `NSException` → `abort` → signal handler → `google_breakpad::ExceptionHandler::WriteMinidumpWithException` → `sentry__breakpad_backend_callback` → `send_envelope_disk_transport`. `/System/Volumes/Data` had ~4.1 GiB free of 1.8 TiB at the time.

## Root cause

The spin is **upstream, in sentry-native 0.7.8**, which Boss statically links through the prebuilt `GhosttyKit.xcframework` (`ghostty` → `pkg/sentry` → sentry-native 0.7.8; `nm` on `libghostty-internal-fat.a` confirms `_sentry__enter_signal_handler`, `_sentry__block_for_signal_handler` and `_g_in_signal_handler` are all in the shipped archive). `src/sentry_sync.c` in that version reads:

```c
static sig_atomic_t g_in_signal_handler = 0;
static sentry_threadid_t g_signal_handling_thread = { 0 };

bool sentry__block_for_signal_handler(void) {
    while (__sync_fetch_and_add(&g_in_signal_handler, 0)) {
        if (sentry__threadid_equal(sentry__current_thread(),
                                   g_signal_handling_thread)) {
            return false;
        }
        sentry__cpu_relax();
    }
    return true;
}

void sentry__enter_signal_handler(void) {
    sentry__block_for_signal_handler();
    g_signal_handling_thread = sentry__current_thread();
    __sync_fetch_and_or(&g_in_signal_handler, 1);
}

void sentry__leave_signal_handler(void) {
    __sync_fetch_and_and(&g_in_signal_handler, 0);
}
```

Four properties combine badly:

1. The wait is an **unbounded busy-spin**. No timeout, no bounded retry, no give-up path. `sentry__cpu_relax()` does not yield, which is why the thread sat at 100 % CPU rather than idling.
2. Its **only** escape, short of the guard being released, is `pthread_equal(pthread_self(), g_signal_handling_thread)` — the sampled leaf frames.
3. The guard is released **only** by `sentry__leave_signal_handler()`, which is the _last_ statement of `sentry__breakpad_backend_callback` — after the minidump is written, the envelope is serialised, `sentry__capture_envelope` has run the disk transport, and the queue has been dumped. Any failure to reach that line latches `g_in_signal_handler` at 1 for the life of the process.
4. `g_signal_handling_thread` is a plain, non-atomic global, written _before_ the flag is raised and **never reset on leave**. Two threads crashing close together can therefore leave the ownership record pointing at a thread that is not the one holding the guard — after which the true owner fails its own same-thread check and spins on itself.

The guard is not only taken on crash entry: `sentry_sync.h` routes `sentry__mutex_lock`, `sentry__mutex_unlock`, `sentry__cond_wait` and `sentry__cond_wait_timeout` through `sentry__block_for_signal_handler()` too. So once the flag is latched, _any_ thread that touches a Sentry mutex joins the spin.

Full disk is the trigger, not the mechanism: it is what made the aborting write fail, what made crash handling unable to complete, and — because Boss has several independent JSONL diagnostics writers, each on its own queue, all of which abort the same way on a failed `FileHandle.write` — what made concurrent aborts likely. The livelock itself is the guard.

### Alternatives considered and rejected

| Hypothesis                                              | Verdict                                                                                                                                                                                                                                                                                                                                                         |
| ------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Genuine spinlock in the re-entrancy guard               | **Confirmed.** `sentry__block_for_signal_handler` is literally an unbounded spinlock and 100 % of samples are inside it.                                                                                                                                                                                                                                        |
| Disk-full re-entry into the signal handler              | Contributing trigger, not the mechanism. The guard does no I/O; what the full disk did was prevent the handler from ever reaching `sentry__leave_signal_handler()`.                                                                                                                                                                                             |
| Signal delivered to a thread that never clears the flag | Supported, and the reason the latch is permanent — see points 3 and 4 above.                                                                                                                                                                                                                                                                                    |
| Known upstream fix available                            | Partly. sentry-native #1446 ("fix: split inproc handler thread", 2026-02-18, post-0.7.8) added a `g_signal_handler_depth` counter so a _recursive_ crash on the owning thread returns a depth instead of spinning. It does **not** bound the cross-thread wait: current `master` still spins forever there. So upgrading would narrow the window, not close it. |

This is upstream C compiled into a binary-only xcframework. It is neither our code nor our Sentry configuration — Boss never calls `sentry_init`; `ghostty_init` does. Since even fixed upstream offers no bounded-time termination guarantee, Boss enforces one itself.

## What the watchdog does

[`CrashWatchdog`](../app-macos/Sources/CrashWatchdog/CrashWatchdog.swift) chains a handler **in front of** whatever Sentry/Breakpad installed, for `SIGABRT`, `SIGBUS`, `SIGFPE`, `SIGILL`, `SIGSEGV` and `SIGSYS`. On a fatal signal it:

1. arms a kernel `alarm()` deadline (once — a cascade of further crashes must not keep pushing it out);
2. unblocks `SIGALRM` on the current thread, because Darwin's `abort(3)` blocks every signal except `SIGABRT` before raising, and a process-directed `SIGALRM` no other thread can accept would simply stay pending forever;
3. calls the previously installed handler, so crash reporting runs exactly as it did before; and
4. if that handler ever returns, restores the default disposition and re-raises, so the process dies with the signal it actually received.

The bound is a **kernel timer plus a default signal disposition**, deliberately not a watchdog thread. A watchdog thread has to be scheduled, and can be blocked behind whatever wedged the crash handler — a held malloc lock, a spinning peer, a starved runloop. `alarm()` needs nothing from userspace once armed.

Every call the handlers make (`sigaction`, `alarm`, `signal`, `pthread_sigmask`, `write`, `raise`, `_exit`) is on POSIX's async-signal-safe list, and all state they touch is allocated during `install()` and reached through raw pointers, so no Swift global initialiser, allocation, or exclusivity check runs on the crash path.

`install()` is called from `GhosttyBootstrap` immediately after `ghostty_init`. The ordering is load-bearing: `ghostty_init` is what initialises Sentry, so installing before it would leave us _behind_ Breakpad in the chain, where an unbounded spin never reaches us.

### Scope

Only signal-delivered faults. On macOS, Breakpad claims hardware faults (`EXC_BAD_ACCESS`, `EXC_BREAKPOINT`, …) through a Mach exception port and handles them on its own thread, so no POSIX signal is generated and this chain never sees them. `abort()` — the reported failure mode, and the terminal path of every uncaught `NSException` — delivers `SIGABRT` to the faulting thread and is fully covered.

## Tuning

`BOSS_CRASH_WATCHDOG_SECONDS` overrides the grace period, in whole seconds. Read once at install. Values are clamped to `[1, 600]`: `0` would _cancel_ the `alarm()` rather than shorten it, and anything past ten minutes recreates the incident. Unset, empty, and unparseable values fall back to the 10-second default, which is roughly an order of magnitude more than writing a minidump plus an envelope takes for a multi-GB-RSS process.

Raising it is reasonable if a legitimate crash report is being cut short (you would see the watchdog message on a crash that _was_ being reported successfully). Lowering it below a couple of seconds risks pre-empting a merely slow crash handler.

## Tests

- `//tools/boss/app-macos/Tests/CrashWatchdog:crash_watchdog_livelock_test` is the reproduction. It injects sentry-native 0.7.8's spin loop as the prior `SIGABRT` handler in a real subprocess and asserts three things: unguarded, that process hangs (so a broken reproduction cannot silently turn the real assertion green); guarded, it dies by `SIGALRM` inside the grace, having still entered the prior handler; and with a well-behaved prior handler it dies immediately by `SIGABRT`, without the watchdog involved.
- `//tools/boss/app-macos/Tests/CrashWatchdog:CrashWatchdogTests` covers the pure parts — grace clamping and the reserved-disposition filter.
