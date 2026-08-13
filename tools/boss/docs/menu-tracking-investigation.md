# Menu tracking sessions in Boss.app — what was measured, and what is still open

Sampling `Boss.app` was reported to always look like a context menu is open:

> A context menu was open for 1661 of 6018 samples (28% of the window) — `NSPopUpButtonCell trackMouse:` → `NSContextMenuTrackingSession` → nested menu event loop.

with no menu visible on screen. This is the second independent report of the same shape. The UI-performance design doc records the first, and had to discard its whole baseline over it:

> A popup / context menu was open for roughly **5087 main-thread samples** of the window (`-[NSPopUpButtonCell trackMouse:]` → `NSMenuTrackingSession`, 1129 of it idle). That is a nested modal event loop across ~11% of the main thread's window, so the baseline is **not** clean steady state.
>
> — [`designs/boss-ui-performance-improvements.md`](designs/boss-ui-performance-improvements.md), "Sampling caveat that constrains every comparison"

This document records what a measurement pass established, what it rules out, and the one step that still needs a human.

## What was measured

All samples are of a live, in-use `Boss.app` (v1.0.537, pid 2246, ~2h uptime) on macOS 26.5.2, 2026-08-12 19:24–19:46 local. Sampling is read-only; nothing was launched, attached to, or restarted.

| Capture                                        | Main-thread samples | `NSPopUpButtonCell trackMouse:` | Menu tracking session |
| ---------------------------------------------- | ------------------- | ------------------------------- | --------------------- |
| `sample 2246 10` @ 19:24                       | 7028                | 0                               | 0                     |
| `sample 2246 30` @ 19:26                       | 21992               | 0                               | 0                     |
| 20 × `sample 2246 5`, ~45 s apart, 19:30–19:46 | ~3600 each          | 0                               | 0                     |
| Fresh isolated instance, zero interaction      | 8886                | 0                               | 0                     |

That is ~140 seconds of on-CPU sampling spread across 22 minutes of wall clock. **No menu tracking session appeared in any of it.**

The 30-second capture is the sharpest one: all 21992 main-thread samples sat in the outer `-[NSApplication run] + 368` → `nextEventMatchingMask` → `mach_msg2_trap` idle path. Not one sample was inside `-[NSApplication _handleEvent:]`, let alone a nested loop.

### What did appear

Short mouse-tracking loops — in the 10-second capture, and in 4 of the 20 watcher samples:

- `-[NSToolbarItemViewer mouseDown:]` → `-[NSCell trackMouse:inRect:ofView:untilMouseUp:]` → `NSControlTrackMouse` → `-[NSDragEventTracker trackEvent:usingHandler:]` — 43 of 7028 samples (0.6%), in the 10-second capture
- `-[NSButtonCell trackMouse:...]` — 43 of 2800 (1.5%) and 54 of 2650 (2.0%)
- `-[NSSegmentedCell trackMouse:...]` — 88 of 3654 (2.4%) and 69 of 3573 (1.9%)

These are ordinary press-and-hold tracking on a toolbar button and on the navigation-mode segmented control, lasting tens of milliseconds each, and they correlate with an actual click. None of them is a pop-up button and none opens a menu — `NSDragEventTracker` is the mouse-down tracker, not a menu session.

## What this rules out

**The session is not permanently active.** A leaked nested modal loop cannot be 28% of a sampling window: once the main thread is stuck inside `trackMouse:`, every subsequent sample is inside it, so a permanent leak reads as 100% from the moment it happens, not 28% and not 11%. Both reported figures describe a session that started and ended (or started partway) inside the window. The 30-second idle capture then confirms it directly — at 19:26 the app had no nested loop at all, on the same build the report came from.

**It is not construction-time.** A fresh instance of the current `main` build, launched with zero user interaction and sampled for 10 seconds, has no tracking session. Whatever produces the condition needs an interaction first. That eliminates every "something opens a menu at startup" hypothesis and narrows the search to interaction-triggered paths.

## What this does not rule out

The reports are real, and 1661 samples is ~2.4 seconds of a nested menu loop. Two readings remain live, and the measurements above cannot separate them:

1. **A genuine menu, open longer than realised** — clicked, then left open while attention moved to the terminal to ask for a sample. Consistent with everything measured; not proven.
2. **A session that outlives its menu** — the menu window is gone (hence "nothing visible") but AppKit's `trackMouse:` loop is still spinning. Also consistent; also not proven. The report that SwiftUI layout was running inside the nested loop does _not_ discriminate: menu tracking runs the main run loop in `NSEventTrackingRunLoopMode`, and SwiftUI's display-cycle observers are registered in the common modes, so a busy app keeps laying out inside a perfectly healthy open menu.

Distinguishing them requires knowing whether a menu was open _at the moment the profile was taken_, which a profile cannot tell you. That is what the instrumentation added alongside this document is for.

## Controls that can produce this stack

`NSPopUpButtonCell` narrows the field considerably: it is a pop-up/pull-down button, which in this app means a SwiftUI `Menu` or a `Picker` in menu style, not a right-click `.contextMenu` and not `NSMenu.popUpContextMenu`. Present in always-visible chrome:

| Control                                                                     | Where                                                             |
| --------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| "New" toolbar menu (New Product / Project / Task / Chore)                   | `tools/boss/app-macos/Sources/ContentView.swift:252`              |
| `WorkGroupToolbarMenu` ("Group by")                                         | `tools/boss/app-macos/Sources/WorkBoardToolbar.swift:272`         |
| `TranscriptView.jumpMenu` ("Jump to turn"), `.menuStyle(.borderlessButton)` | `tools/boss/app-macos/Sources/TranscriptView.swift:132`           |
| Comment sidebar menu                                                        | `tools/boss/app-macos/Sources/Comments/CommentSidebar.swift:332`  |
| Worker pool `Picker` (menu style by default)                                | `tools/boss/app-macos/Sources/Ghostty/WorkersDetailView.swift:93` |

Not on the list, despite being named as candidates in the report: `UpdateBadgeToolbarButton` (`ContentViewChrome.swift:371`) and `WorkProjectFilterToolbarButton` (`WorkBoardToolbar.swift`) are `Button` + `.popover`, which is `NSButtonCell`, not a pop-up button; `WorkBoardCard`'s card menu (`WorkBoardCard.swift:231`) is `.contextMenu`; the Ghostty terminal (`GhosttyTerminalView.swift:721`) and comment layer (`CommentLayer.swift:1144`) call `NSMenu.popUpContextMenu` / `NSMenu.popUp` directly. All of those produce a different top frame than the one reported.

A live profile taken while the condition holds should show `MenuPickerStyle` or the toolbar item in the frames just below `trackMouse:`; the first 10-second capture did show `MenuPickerStyle.Body.popUpButton(style:)` inside a normal `NSHostingView.layout()` pass, confirming a menu-style `Picker` is live in the view graph, but a body evaluation is not a tracking session and does not identify the control.

## Why an agent cannot close this

Reproducing the condition needs a mouse on a real window. Every route an agent has is blocked:

- The sanctioned isolated capture instance **does not create a window at all** on macOS 26.5.2. The documented recipe fails outright:

  ```
  $ BOSS_SOCKET_PATH=/tmp/boss-shot-<id>.sock BOSS_ENGINE_AUTOSTART=0 \
      bazel run //tools/boss/app-macos:Boss -- --capture-to /tmp/probe.png --capture-after 5
  Boss capture: capture failed: NSApp.windows is empty (no WindowGroup window created)
  ```

  This is the `NSApp.windows`-empty regression `BossCapture.swift` already tolerates as intermittent, arriving as total. Confirmed under `lldb` against a long-lived isolated instance: `[[NSApplication sharedApplication] windows]` is an empty array minutes after launch, and stays empty after forcing `.regular` activation policy while hidden. So there is no window to click, and no live AppKit state to inspect.

- Accessibility automation, which can drive a non-frontmost window, is unavailable: `System Events got an error: osascript is not allowed assistive access. (-25211)`.
- Driving the live running app in front of the person using it, or launching a visible instance on their laptop, is forbidden by the worker rules — and the UI-performance doc's measurement protocol says the same thing in more detail: "Every step in this protocol is a human step. An agent implementing any task in this plan must not attempt it."

## What the app now records

[`MenuTrackingMonitor`](../app-macos/Sources/Diagnostics/MenuTrackingMonitor.swift) observes `NSMenu.didBeginTrackingNotification` / `didEndTrackingNotification` and mirrors each transition into the existing `terminal-loop-*.jsonl` diagnostics stream as a `menu_tracking` record. It costs two notification observers while idle; the 1 Hz probe that emits `still_open` records exists only while a menu is open, and is registered in `.common` run loop modes so it keeps firing inside the nested tracking loop instead of going silent for exactly the interval it exists to observe.

Reading it:

```sh
grep menu_tracking ~/Library/Application\ Support/Boss/terminal-loop-$(date -u +%F).jsonl
```

- A `begin` with a matching `end` — a menu somebody opened and closed. `open_ms` says for how long.
- A `begin` with no `end`, followed by `still_open` records whose `open_ms` keeps growing — an orphaned session, and `menu` names the control. Because SwiftUI's `Menu` bridge leaves `NSMenu.title` empty, the label is derived from the item titles, so an orphan reads as e.g. `"New Product / New Project / New Task / New Chore"` rather than `"(untitled)"`. A session past 30 s also emits an `os_log` notice under subsystem `com.boss.app`, category `menu-tracking`.
- A profile showing a nested menu loop while this log has **nothing** open is the third case: an AppKit loop that outlived its menu. That is the one the notifications cannot see directly, and the `runloop_mode` field on each record is what pins it — it captures whether the main run loop was in `NSEventTrackingRunLoopMode` at the moment of the record.

This is deliberately additive. The report is the messenger; nothing here filters, suppresses, or special-cases menu state out of any profile or diagnostic.

## The step that still needs a human

Next time a sample shows the nested loop, do not reason from the profile — read the `menu_tracking` lines covering the same wall-clock window. That settles reading 1 vs reading 2 in one grep, and if it is reading 2, it names the control. Until then this stays open: the mechanism is not identified, and no control has been changed on the strength of a guess.
