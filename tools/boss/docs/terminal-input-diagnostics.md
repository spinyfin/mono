# Terminal keyboard-input diagnostics

Instrumentation for the "typed into a Boss terminal pane, heard a beep,
the keystroke never arrived" report (intermittent, under heavy machine /
engine load, coordinator pane). It ships always-on in the macOS app and
exists so the next occurrence can be attributed from a log rather than
re-investigated from memory.

**Workers:** the files live under the Boss state root, which is
engine-owned and off limits to worker sessions. Read this doc to know
what the surface can answer; do not open the files from a worker.

## Where it is

- Day-rotated JSONL under the state root:
  `diagnostics/terminal-input-YYYY-MM-DD.jsonl` (7 days retained).
- Read with `bossctl logs terminal-input` (same flags as the other
  sources: `--since 10m`, `--grep`, `--field key=value`, `--follow`).
- Every line is also emitted to unified logging, so it can be watched
  live while reproducing:

  ```sh
  log stream --predicate 'subsystem == "com.boss.app" AND category == "terminal-input"'
  ```

Keystroke _content_ is never recorded. Letters, digits, symbols and space
are reduced to a class (`letter` / `digit` / `symbol` / `space`) and carry
no `key_code` (a virtual key code identifies the physical key); only named,
control and function keys are identified (`return`, `escape`, `up_arrow`,
`control_3`, …) and carry a `key_code`. Typing into a legitimate text
field in the same window is coalesced to a count per focus episode.

Writers: `TerminalInputLog` (file + os_log), `TerminalInputMonitor`
(window / key / stall / libghostty-log observers), the pane view
`GhosttyTerminalHostView` (focus, attach/detach, per-key outcomes) and
`GhosttyRuntime` (bells). All in `tools/boss/app-macos/Sources/`.

## Why two different beeps sound the same

Both candidate beep sources end in the **same system alert sound**:

1. **AppKit's unhandled-key beep.** A `keyDown` goes to the window's
   first responder. If that is not the terminal view and nothing in the
   responder chain handles the key, `NSResponder.noResponder(for:)` calls
   `NSBeep`. One event: the key is dropped _and_ the alert plays.
2. **A terminal BEL.** libghostty forwards every BEL from a pane's pty as
   `GHOSTTY_ACTION_RING_BELL`; `GhosttyRuntime` plays `NSSound.beep()`
   for the coordinator pane only (worker bells are muted by
   `shouldRingBell`). A BEL does not by itself drop input.

The log separates them: every BEL produces a `bell` line with
`rang_system_alert`; a beep with **no** `bell` line within a few hundred
milliseconds is AppKit's, and there should be a `no_responder_window` line
(the window's responder chain ended unhandled) next to it instead.

## Event vocabulary

Every line has `ts_epoch_ms` and `event`. Pane-scoped lines carry
`pane` (session id: `boss`, or `run-<runId>`), `role` (`boss` /
`worker`) and `slot` for workers.

| `event`                      | Emitted when                                                                                                                                                                                                                             | Key fields                                                                                                           |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `monitor_started`            | App launch (non-isolated instances only).                                                                                                                                                                                                | `stall_threshold_ms`, `heartbeat_interval_ms`                                                                        |
| `terminal_window_registered` | A pane first joins a window; KVO on that window's `firstResponder` begins.                                                                                                                                                               | `window`, `first_responder`                                                                                          |
| `first_responder_changed`    | The window's first responder moved (KVO).                                                                                                                                                                                                | `old`, `new`, `old_kind`, `new_kind` (`terminal` / `text_input` / `window` / `other` / `none`), `is_key_window`      |
| `key_window_changed`         | A terminal-hosting window became / resigned key.                                                                                                                                                                                         | `became_key`, `first_responder`, `app_key_window`                                                                    |
| `key_not_delivered`          | Routing context: a `keyDown` arrived in a terminal-hosting window while the first responder was **not** a terminal pane and **not** a text field. Logged before dispatch; it does not say the key was unhandled or that anything beeped. | `responder`, `responder_kind`, `key`, `key_code` (named keys only), `mods`, `is_repeat`, `since_responder_change_ms` |
| `no_responder_window`        | A terminal-hosting window's responder chain ended in `noResponder(for:)` — **the AppKit beep site** when focus has left the pane. Carries the most recent redacted key.                                                                  | `selector`, `responder`, `responder_kind`, `key`, `key_code` (named keys only), `mods`, `since_last_key_ms`          |
| `key_to_text_input`          | Keys went to a legitimate text field; one line per focus episode.                                                                                                                                                                        | `responder`, `count`, `episode_ms`                                                                                   |
| `terminal_focus`             | A pane became / resigned first responder (pane-side view of the KVO line).                                                                                                                                                               | `change` (`become` / `resign`), `accepted`, `has_surface`                                                            |
| `host_window_detached`       | A pane's NSView is leaving its window. AppKit resets the first responder when the responder's view leaves — if `was_first_responder` is true, focus is lost here.                                                                        | `window`, `new_window`, `was_first_responder`                                                                        |
| `host_superview_changed`     | A pane's NSView is being re-parented inside its window (same hazard).                                                                                                                                                                    | `removed`, `was_first_responder`                                                                                     |
| `host_window_attached`       | A pane's NSView joined a window.                                                                                                                                                                                                         | `window`, `is_first_responder`, `is_key_window`                                                                      |
| `key_dropped_no_surface`     | A press reached a pane with no live libghostty surface. Silent to the user.                                                                                                                                                              | `key`, `key_code` (named keys only), `mods`                                                                          |
| `key_not_consumed`           | `ghostty_surface_key` returned false for a press/repeat: nothing was queued for the pty (`had_text` marks printable keys).                                                                                                               | `key`, `key_code` (named keys only), `mods`, `had_text`                                                              |
| `do_command`                 | AppKit routed a `doCommand(by:)` selector to a pane (the `NSTextInputClient` path; Boss never calls `interpretKeyEvents`, so this should not happen).                                                                                    | `selector`                                                                                                           |
| `no_responder`               | An event reached a pane with no handler (AppKit beeps for `keyDown:`).                                                                                                                                                                   | `selector`                                                                                                           |
| `bell`                       | A BEL arrived from a pane's pty.                                                                                                                                                                                                         | `rang_system_alert`                                                                                                  |
| `main_thread_stall`          | A 100 ms main-queue heartbeat fired late enough that the main thread was unavailable for more than 250 ms. No backtrace (see below).                                                                                                     | `blocked_ms`, `key_window`, `first_responder`                                                                        |
| `libghostty_log`             | libghostty wrote a warning/error to unified logging (`com.mitchellh.ghostty`). Its pty writer logs `write error: …` here when a pty write fails.                                                                                         | `category` (Zig log scope, e.g. `io_exec`), `level`, `message`                                                       |
| `libghostty_log_dropped`     | More than 20 libghostty warnings/errors arrived in one 2 s poll; the overflow was dropped (pty write errors never are).                                                                                                                  | `count`, `first_logged_at_epoch_ms`, `last_logged_at_epoch_ms`                                                       |
| `libghostty_log_mirror_*`    | The unified-log mirror could not open or read the store (once).                                                                                                                                                                          | `error`                                                                                                              |

What is **not** observable from the app: a _short_ write to the pty.
libghostty's writer ignores the byte count its write callback returns
and logs nothing for a partial write, and the C API returns `void` from
`ghostty_surface_text` and only consumed / not-consumed from
`ghostty_surface_key`. Logging the requested vs. written byte counts
needs a patch to `ttyWrite` in the `spinyfin/ghostty-prebuilts` fork and a
new GhosttyKit prebuilt (see `runbooks/update-ghostty-prebuilt.md`); that
is not part of the app-side instrumentation.

## Reading an incident

A beep is heard and a key is missing at time T:

```sh
bossctl logs terminal-input --since 5m
```

1. **Is there a `bell` line at ~T with `rang_system_alert: true`?**
   The beep was a terminal BEL (program / tmux / shell), not AppKit. Look
   at the pane's program for why it rang; the drop is a separate question
   (check `key_not_consumed`, `main_thread_stall`, `libghostty_log`).
2. **Is there a `no_responder_window` line at ~T?** AppKit beeped: the key
   reached the end of the responder chain unhandled. A `key_not_delivered`
   line alone is only routing context (the key went to `responder`
   instead of the terminal; a non-terminal responder may well have
   handled it), so require the `no_responder_window` line before calling
   it a beep. Read the `first_responder_changed` lines just before it:
   - `old_kind: terminal` → `new_kind: other` with a small
     `since_responder_change_ms` on the dropped key means a transient
     first-responder move during a re-render. A preceding
     `host_window_detached` / `host_superview_changed` with
     `was_first_responder: true` names the mechanism (SwiftUI re-parented
     the pane's NSView; AppKit reset focus).
   - `new_kind: window` means focus was dropped to the window itself —
     classic "nothing has focus, every key beeps". (`new_kind: none` is a
     nil first responder.)
   - `key_window_changed` with `became_key: false` at ~T means another
     window took key (panel, popover, alert) — the key went there.
3. **Is there a `main_thread_stall` at ~T?** Correlate with the engine
   trace (`bossctl logs engine --since …`) for the update burst. A stall
   alone does not drop keys (AppKit queues them), but it changes _when_
   they are delivered and to _what_. For a backtrace, enable the opt-in
   `MainThreadStallMonitor` (Settings → Feature Flags → UI stall
   monitoring) and read the UI Stalls window.
4. **Is there a `libghostty_log` with `write error` at ~T?** The pty
   rejected the write — tmux's client was not reading (backpressure) or
   the pty was gone. That is the pty-path drop; the beep still needs one
   of the explanations above.
5. **`key_not_consumed` with `had_text: true`** means libghostty refused a
   printable key. Check the user's own Ghostty config (Boss loads
   `~/.config/ghostty/config`): a keybind matching the chord consumes it
   and the bound action is handed to Boss, which ignores most actions.

## Reproduction notes (2026-10-05)

This instrumentation was landed without a reproduction: the symptom needs
an interactive session typing into the real coordinator pane under load,
which a headless worker cannot drive (an app launch would put a window on
the user's screen and take focus). Suggested recipe:

```sh
# CPU saturation (one per core):
for i in $(seq 1 "$(sysctl -n hw.ncpu)"); do yes > /dev/null & done
# plus an engine update burst, e.g. several workers starting/finishing
# at once, then type steadily into the coordinator pane and watch:
log stream --predicate 'subsystem == "com.boss.app" AND category == "terminal-input"'
# afterwards:
killall yes
```

## Deliberately not done

The beep is the signal. Nothing here mutes it, swallows unhandled keys,
retries keys, or changes focus behaviour. A fix (keeping the pane first
responder across a re-render, or a libghostty-side pty write retry) should
follow from what these lines show, not precede it.
