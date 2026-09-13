# Segmented `Picker` `_overrideSizeThatFits` cost

## Question

Why did segmented `Picker`s dominate Boss main-thread time under ordinary use,
and what replaces them without changing selection, keyboard, or accessibility
behaviour?

## Verdict

`.pickerStyle(.segmented)` is an `NSViewRepresentable`. Its
`_overrideSizeThatFits` re-enters SwiftUI with a nested ViewGraph update on
every layout pass of the enclosing view, re-installing the observation
key-path set. Observation plus `AnyKeyPath` hashing was ~43–56% of main-thread
self time in the profiles that motivated this change.

The two sites that accounted for the 31–45% share — the toolbar Mode picker
and the Workers pool picker — now use one shared SwiftUI-native control,
`NativeSegmentedPicker`: an `HStack` of buttons with a selection pill, no
`NSViewRepresentable`, no `NSSegmentedControl`.

## Measured impact (before)

Three `sample` profiles of the running Boss app under normal use:

| Attribution (one profile) | Share of main-thread time |
| ------------------------- | ------------------------: |
| Workers pool picker       |                     29.8% |
| Toolbar Mode picker       |                     13.7% |
| UI Stalls "Since" picker  |                      1.1% |
| Boss's own Swift          |                  0.1–0.2% |

The Mode picker's labels are static, so this is not a "derived title"
problem. One profile was captured while the operator was dragging a
scrollbar; the time still went to segmented-picker measurement. The cost is
per layout pass of the representable, multiplied by the enclosing view's
invalidation rate.

## Replacement

`tools/boss/app-macos/Sources/NativeSegmentedPicker.swift`, used at:

- `ContentView` toolbar Mode picker (440pt frame, `navigationMode` binding)
- `WorkersDetailView` pool picker (`maxWidth: 460`, live titles with pool
  counts, shares the row with `LegacyHostingBadge`)

Behaviour preserved:

- Selection binds to the existing model. A value that is no longer in the
  option list is left alone (same as `Picker`).
- Arrow keys move without wrapping; VoiceOver increment/decrement does the
  same. The control is one keyboard focus target.
- Each segment keeps an accessible label; selected state is
  `.isSelected`; VoiceOver increment/decrement moves the selection.

Unit tests pin the mechanism: `NativeSegmentedPicker` installs no
`NSSegmentedControl` even across 200 relayouts; a system segmented `Picker`
still does.

## What this does not cover

Other `.pickerStyle(.segmented)` sites (UI Stalls "Since", Attentions,
Settings, Ideas, Activity log, Terminal Loop, editorial sheet, work form
sheets) were not the measured 31–45%. They can take the same control later.

## Live-app sample (operator)

Isolated `--capture-to` cannot reproduce the original load (populated board,
live workers, scrollbar drag). Agents must not launch the production
Boss.app. A person captures the before/after pair:

```sh
# Boss frontmost, Agents tab, stall monitoring on, no menu or popover.
sample <Boss_pid> 60 -file /tmp/boss-segmented-before.txt
# after this change is installed
sample <Boss_pid> 60 -file /tmp/boss-segmented-after.txt
```

Symbols to read: `_overrideSizeThatFits`, `NSSegmentedControl`,
`SegmentedPickerStyle`, `AG::Graph` / `AnyKeyPath` hashing, main-thread
on-CPU (total minus `mach_msg2_trap`).

Prediction: Mode + Pool contribute ~0 inclusive samples under
`NSSegmentedControl` / `_overrideSizeThatFits`. The 31–45% share should move
to ordinary SwiftUI `Button` / `Layout` / `Text` frames at far lower cost.
The UI Stalls "Since" picker is unchanged and will still show the
representable path if that window is open.
