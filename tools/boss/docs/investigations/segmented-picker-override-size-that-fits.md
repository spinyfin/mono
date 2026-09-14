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
and the Workers pool picker — now use one shared AppKit wrapper,
`NativeSegmentedPicker`: an `NSViewRepresentable` around `NSSegmentedControl`
that sets plain string labels via `setLabel(_:forSegment:)` and never hosts
SwiftUI content per segment. Measurement is `sizeThatFits(_:nsView:context:)`
calling AppKit `fittingSize` only; it does not re-enter the ViewGraph.

The slow path was not "any `NSViewRepresentable`". It was this particular
representable hosting SwiftUI labels. The codebase already has several thin
AppKit wrappers (`CommentTextEditor`, `ResizeDivider`, `GhosttyTerminalView`)
that do not pay the nested-ViewGraph cost.

A prior SwiftUI-only approximation (`HStack` of buttons, no AppKit control)
avoided the measurement cost but painted a click-to-focus ring around the
whole control and guessed at track/pill metrics. The AppKit wrapper makes
both structurally impossible.

## Measured impact (before)

Three `sample` profiles of the running Boss app under normal use, captured
against `.pickerStyle(.segmented)`:

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
- Keyboard and accessibility behaviour is `NSSegmentedControl`'s own:
  VoiceOver exposes the control as a segment group and selects segments
  directly (there is no increment/decrement action, unlike the previous
  SwiftUI implementation), and arrow-key movement applies when the control
  holds first responder under Full Keyboard Access. A mouse click takes
  AppKit's click-focus semantics and paints no focus ring around the whole
  control.
- Each segment's accessible label is the string passed to `setLabel`.
- Unbounded proposals (NSToolbar's measure pass) report AppKit's label
  ideal, not a poisoned infinite width. Finite proposals, including 0,
  are filled. Unit tests pin `NSHostingView.fittingSize` in the
  label-sized range and pin labels via `setLabel(_:forSegment:)` on a
  single `NSSegmentedControl`.

Do not revert these two sites to `Picker` + `.pickerStyle(.segmented)`.

## What this does not cover

Other `.pickerStyle(.segmented)` sites (UI Stalls "Since", Attentions,
Settings, Ideas, Activity log, Terminal Loop, editorial sheet, work form
sheets) were not the measured 31–45%. They can take the same control later.

## Live-app sample

Isolated `--capture-to` is an offscreen `cacheDisplay` that exits and cannot
reproduce the original load (populated board, live workers, scrollbar drag).
Capture the before/after pair against a live app:

```sh
# Boss frontmost, Agents tab, stall monitoring on, no menu or popover.
sample <Boss_pid> 60 -file /tmp/boss-segmented-before.txt
# after this change is installed
sample <Boss_pid> 60 -file /tmp/boss-segmented-after.txt
```

Symbols to read: `_overrideSizeThatFits`, `NSSegmentedControl`,
`SegmentedPickerStyle`, `AG::Graph` / `AnyKeyPath` hashing, main-thread
on-CPU (total minus `mach_msg2_trap`).

Prediction: Mode + Pool still contribute ~0 inclusive samples under
Picker-style `_overrideSizeThatFits` / `SegmentedPickerStyle` / `AnyKeyPath`
hashing. `NSSegmentedControl` will appear (it is the wrapper's view) but
must not dominate main-thread self time. The UI Stalls "Since" picker is
unchanged and will still show the representable path if that window is
open.
