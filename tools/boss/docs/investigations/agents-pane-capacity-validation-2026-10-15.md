# Validating the Agents pane capacity constants on real displays

- **Date:** 2026-10-15
- **Kind:** measurement pass over `PaneCapacityPolicy` (`tools/boss/app-macos/Sources/PaneLayout/PaneCapacityPolicy.swift`) against the two displays attached to the operator's machine
- **Project:** Dynamic Agents pane layout — [`designs/dynamic-agents-pane-layout.md`](../designs/dynamic-agents-pane-layout.md), shipped in `mono#3170`
- **Host:** 16-inch MacBook Pro (Apple M2 Max), macOS 27.0.1 (26A434); built-in display 1728×1117 pt at 2x, LG HDR WQHD 3440×1440 pt at 1x
- **Ghostty:** the GhosttyKit prebuilt pinned in `MODULE.bazel` (commit `71c2d68e`), default font (JetBrains Mono); both user Ghostty config files on the host contain no active `font-*` lines, so the probe and the app resolve the same font
- **Attachments:** one capture on the work item (the fixture-window path rendering on this host). No Agents-grid capture exists; see [What could not be measured](#what-could-not-be-measured)

## Verdict

No constant needs to change. The terminal cell libghostty reports at the fixed 10pt worker font is exactly 6×13 pt on both the 2x laptop display and the 1x external display, which is the value the design estimated. The two-line pane header measures 41 pt against the 44 pt estimate; the 3 pt of slack is conservative and does not move any row threshold on either display. With those inputs the policy gives 8 panes (4×2) on the maximized laptop window, 16 panes (the cap, 6×3 rendered) on the full-width external display, and 12 (4×3) on a half-width external window. Capacity is width-bound on every geometry measured, so `minColumns` is the only constant that decides capacity; `minRows` never binds.

The capture half of the brief — the Agents grid at one, four, eight and capacity agents with a viewer-missing card — could not be produced: no capture fixture populates the Agents grid, and the plain `--capture-to` path creates no window on this macOS build. The follow-ups at the end specify the fixture that would close that gap.

## What was asked, and what this pass could do

| Item in the brief                                                 | Status                                                                                                                                                                               |
| ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Measured cell size at the worker font                             | **Measured** directly from libghostty (the same static library the app links), at 2x and 1x                                                                                          |
| Capacity on laptop and large-display geometries                   | **Computed** by compiling the repo's `PaneCapacityPolicy.swift` unchanged into a scratch calculator and feeding it the measured cell, the measured chrome and the real screen frames |
| Header height (`headerHeight`, documented as "not yet measured")  | **Measured on a replica** of `WorkerSlotView.slotHeader` hosted in `NSHostingView`; the real view cannot be instantiated outside the app                                             |
| Captures at 1 / 4 / 8 / capacity agents, laptop and large display | **Not produced.** No fixture feeds synthetic agents into the Agents grid, and the plain capture path produces no window on macOS 27.0.1 (details below)                              |
| Viewer-missing card capture                                       | **Not produced**, same reason                                                                                                                                                        |
| Legibility                                                        | **Assessed from geometry only** (pane size in columns×rows and physical glyph size); no rendered pane was inspected                                                                  |
| Tune constants with matching model tests                          | **Not needed**; nothing measured contradicts the current values                                                                                                                      |

## Method

### Cell size: a headless libghostty probe

A scratch Swift program linked `libghostty-internal.a` from the Bazel-fetched `GhosttyKit.xcframework` and replayed the app's own surface setup from `GhosttyRuntime.swift` and `GhosttyTerminalView.swift`: strip `GHOSTTY_*` from the environment, `ghostty_init`, load the default and recursive config files, `ghostty_app_new`, then for each case a never-shown `NSWindow` hosting a layer-backed `NSView`, `ghostty_surface_new` with `font_size` and `scale_factor` set, `ghostty_surface_set_content_scale`, `ghostty_surface_set_size`, one `ghostty_app_tick`, and `ghostty_surface_size()`. The surface command was `/bin/sleep 30` so no login shell ran. Every `GHOSTTY_ACTION_CELL_SIZE` action was logged as well, which is the channel the app actually consumes.

### Chrome and header

The toolbar height and the Agents header strip come from a real isolated capture plus a replica. The capture used the reveal fixture (`--capture-reveal-state=done`), which hosts the real `ContentView` in its own window; its pixel rows put the unified toolbar's bottom divider at 104 px, i.e. 52 pt. The Agents header strip (`WorkersDetailView.agentsHeader` plus its `Divider`) and the two-line pane header (`WorkerSlotView.slotHeader` plus its `Divider`) were rebuilt view-for-view in scratch programs and measured with `NSHostingView.fittingSize`. Both replicas copy the fonts, paddings, spacings and fixed frames from the source; they do not share code with it, so they are a faithful estimate rather than a measurement of the shipped view.

### Capacity

`PaneCapacityPolicy.swift` was compiled unmodified with a small `main.swift` that prints `limits`, `capacity`, and the `shape` the policy chooses for used spans 1, 2, 4, 8 and capacity. Pane sizes in the tables below use the 1 pt gap that `WorkersDetailView.placement` actually renders, not the policy's 4 pt `gap` (see observations). Terminal columns and rows are the pane size divided by the measured cell, after subtracting the header.

Pane area per geometry is the window frame minus the 52 pt toolbar and the 37 pt Agents header strip. Agents mode is the one top-level mode that is not a `NavigationSplitView` (`ContentView.agentsView`), so it has no sidebar and the full window width is available. Screen frames came from `NSScreen.screens`: the laptop's visible frame is 1728×1084 (menu bar deducted), the LG's is the full 3440×1440.

## Measurements

### Terminal cell size (libghostty, this pin, default font)

| Font size | Scale | Cell in pixels | Cell in points | Note                                             |
| --------- | ----- | -------------- | -------------- | ------------------------------------------------ |
| 10 pt     | 2x    | 12×26          | **6.0×13.0**   | worker pane on the built-in display              |
| 10 pt     | 1x    | 6×13           | **6.0×13.0**   | worker pane on the LG                            |
| 11 pt     | 2x    | 13×29          | 6.5×14.5       | coordinator pane (`BossPaneModel`), for contrast |
| 11 pt     | 1x    | 7×15           | 7.0×15.0       | coordinator pane on the LG                       |

The design's 6×13 estimate is exact at the worker font on both scale factors. The unit tests in `PaneLayoutFixtures.swift` already use 6×13.

Each surface emitted two `CELL_SIZE` actions during creation: first 8×17 pt (libghostty's default 13 pt font, before `font_size` is applied), then the 10 pt value. The app records whichever arrives last, and both arrive synchronously inside `ghostty_surface_new`, so the layout model ends on the right value; this is noted under observations because a future ordering change would silently halve capacity.

### Chrome and header

| Element                                            | Value | Source                             | Policy constant   |
| -------------------------------------------------- | ----- | ---------------------------------- | ----------------- |
| Unified toolbar (title bar row with the mode tabs) | 52 pt | pixel scan of the attached capture | n/a               |
| Agents header strip (`agentsHeader` + divider)     | 37 pt | replica, `fittingSize`             | n/a               |
| Two-line pane header (`slotHeader` + divider)      | 41 pt | replica, `fittingSize`, 420–860 pt | `headerHeight` 44 |
| Rendered gap between panes                         | 1 pt  | `WorkersDetailView.placement`      | `gap` 4           |

The pane header is 28 pt of portrait plus 12 pt of vertical padding plus the 1 pt divider; the two caption lines (13 pt each plus 1 pt spacing) are shorter than the portrait and do not drive the height. Both `.caption` and `.caption2` resolve to 10 pt with a 13 pt line height on this macOS, so a recovery banner on the second line does not grow the header either.

### Capacity by geometry

Pane area = window width × (window height − 52 − 37). Minimum pane at the measured cell is 420×356 pt (70×6 by 24×13+44). Terminal size per pane is after the 44 pt header; using 41 pt instead adds at most one row and changes no shape.

**Laptop, maximized (1728×1084 frame, pane area 1728×995):** limits 4×2, **capacity 8**.

| Agents | Shape | Pane (pt) | Terminal (cols×rows) |
| ------ | ----- | --------- | -------------------- |
| 1      | 1×1   | 1728×995  | 288×73               |
| 2      | 2×1   | 863×995   | 143×73               |
| 4      | 2×2   | 863×497   | 143×34               |
| 8      | 4×2   | 431×497   | 71×34                |

**Laptop, default window (1060×680 content, pane area 1060×643):** limits 2×1, **capacity 2**. One agent gets 176×46; two get 88×46 side by side; a third agent opens page 2.

**Laptop, 1400×900 window (pane area 1400×811):** limits 3×2, **capacity 6**; at six agents each pane is 77×27.

**LG, full width (3440×1440, pane area 3440×1351):** limits 8×3 (24 would fit), **capacity 16 — the `maxPanesPerPage` cap binds**.

| Agents | Shape | Pane (pt) | Terminal (cols×rows) |
| ------ | ----- | --------- | -------------------- |
| 1      | 1×1   | 3440×1351 | 573×100              |
| 2      | 2×1   | 1720×1351 | 286×100              |
| 4      | 4×1   | 859×1351  | 143×100              |
| 8      | 4×2   | 859×675   | 143×48               |
| 16     | 6×3   | 573×450   | 95×31                |

**LG, half width (1720×1440, pane area 1720×1351):** limits 4×3, **capacity 12**.

| Agents | Shape | Pane (pt) | Terminal (cols×rows) |
| ------ | ----- | --------- | -------------------- |
| 1      | 1×1   | 1720×1351 | 286×100              |
| 4      | 2×2   | 860×675   | 143×48               |
| 8      | 3×3   | 573×450   | 95×31                |
| 12     | 4×3   | 429×450   | 71×31                |

**LG, two-thirds width (2293×1440, pane area 2293×1351):** limits 5×3, **capacity 15**; at fifteen agents each pane is 76×31.

### Threshold margins and the dead band

`deadBand` is 16 pt. The nearest threshold to a real geometry is the LG full-width 8-column edge: eight minimum panes need 3388 pt and the window offers 3440, a 52 pt margin, so the capacity does not flap when the window is tiled by a few points. The laptop maximized window is 81 pt short of a third row (1076 pt needed, 995 available) and far from a fifth column (2116 pt needed). A title-bar accessory banner (`windowTitlebarAccessory`) shrinks the content height and could move the laptop further from, never closer to, the third row.

## Legibility assessment

This is geometric, not visual: no rendered worker pane was inspected.

- **At the minimum pane** (70×24): on the built-in display a 10 pt JetBrains Mono cap height is about 7.3 pt, 1.5 mm at 254 ppi. On the LG the same 10 pt renders at 10 px with no 2x supersampling, about 1.7 mm at 110 ppi, so it is physically slightly larger but drawn with a 1x glyph raster. Whether the 1x raster at 10 pt reads comfortably is exactly the thing a capture would settle and this pass could not.
- **Width is the binding constraint everywhere.** At capacity the laptop gives 71 columns, the LG half-window 71, the LG full window 95; rows at capacity are 31–34, well above the 24 minimum. Raising `minRows` would change nothing on these displays; raising `minColumns` to 80 would drop the maximized laptop from 4×2 to 3×2 (capacity 6) and the LG full window from 8 to 7 columns (still capped at 16). The design's rationale for 70 (the operator's existing four-column laptop layout) is still the only evidence for it; an 80-column floor is the obvious alternative if captured panes show Claude Code wrapping its tool output at 71 columns.
- **The default window is the surprise.** A fresh 1060×680 window holds two agents per page. The design's "eight-pane laptop layout" holds only for a maximized window; an operator who keeps Boss at the default size will see pagination from the third agent.
- **The viewer-missing card** has no terminal, so its legibility is set by the 220×240 pt portrait and `.title3` name; at the smallest pane on these displays (429×450 pt) the card fits without clipping. This is a geometric statement about the card's fixed frames, not an inspection.

## What could not be measured

- **Agents-grid captures with live or synthetic agents.** The isolated capture instance has no engine, so the Agents view renders its empty state. The existing fixtures (`--capture-reveal-state=…`) populate the work board through `ChatViewModel.applyEventForTest` and have no equivalent that feeds `LiveWorkerStateStore`, which is what `WorkersDetailView` reads. Without that there is no 1 / 4 / 8 / capacity capture and no viewer-missing card capture.
- **The plain `--capture-to` path on this host.** Four launches (two with `--capture-after 2`) all ended with `capture failed: NSApp.windows is empty (no WindowGroup window created)` after the eight-attempt retry loop. The `WindowGroup` command-line-launch regression tracked as FB21087054 is persistent on macOS 27.0.1 rather than intermittent. The fixture-window path, which creates its own `NSWindow` and hosts `ContentView` explicitly, rendered first time (the attached capture).
- **Window geometry.** Even with a fixture, the capture window is hard-coded at 1060×680; there is no flag to request a laptop-maximized or 3440×1440 window, so "laptop and large-display geometries" cannot currently be captured, only computed.
- **The shipped header view.** 41 pt comes from a replica, not from `WorkerSlotView` itself.

## Observations beyond the constants

1. **Gap mismatch.** `PaneCapacityPolicy.gap` is 4 pt and feeds both the column/row count and `fitScore`, but `WorkersDetailView.placement` renders a 1 pt gap. The count is marginally conservative (it assumes 3 pt more spacing per pane than exists) and the rendered pane is up to 3 pt wider than the policy assumed. No capacity changes on the geometries above, but the two should agree.
2. **Two `CELL_SIZE` actions per surface.** The first carries the default-font cell (8×17 pt). The app's handler overwrites `lastCellSize` on each action, so the final value is right, but `AgentsGridModel.noteCellSize` runs twice and the first run applies geometry at the wrong cell. The dead band and the fact that both happen before any member is placed make it harmless today.
3. **`headerHeight` is now measured (41 pt, replica)**; the doc comment on the constant still says "not yet measured". Keeping 44 is fine and slightly conservative.
4. **The design's 1x assumption holds.** `GhosttyTerminalView` passes `NSScreen.main`'s scale at creation and later syncs `ghostty_surface_set_content_scale` from the window's actual backing scale, and the cell comes out identical at both scales, so a window dragged between the two displays keeps its capacity.

## Follow-ups (code; out of scope for this investigation)

1. **Agents-grid capture fixture.** Add `--capture-agents=<n>` (and `--capture-viewer-missing=<k>`, `--capture-window=<w>x<h>`) next to `--capture-reveal-state`, hosted the same way `BossRevealCapture` hosts `ContentView` in its own `NSWindow`, feeding `n` synthetic `WorkerLiveState`s (distinct personas, mixed types and projects, `k` of them with no viewer) into `LiveWorkerStateStore` and switching `navigationMode` to `.agents`. The viewer-less cards render without libghostty, so one fixture produces every requested capture; attached terminals would need the isolated engine and are not required to validate the constants. Run it at 1728×1084 and 3440×1440 for 1, 4, 8 and capacity agents, attach the captures, and revisit the 70-column floor with real rendered panes.
2. **Align the gap.** Either render `policy.gap` in `WorkersDetailView.placement` or set `gap` to 1 so the policy counts what is drawn.
3. **Update the `headerHeight` comment** to cite this measurement.
4. **Make the capture path robust on macOS 27.** `--capture-to` without a fixture should fall back to the fixture-window hosting instead of retrying `NSApp.windows` eight times.
