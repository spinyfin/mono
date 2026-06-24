# Boss markdown render slowness — 2026-05-18

Investigation of the ~129 s render time Boss reports for the 46 KB editorial-controls design doc (mono#688). PR #686's Textual-only rig already established that **Textual itself is not the bottleneck** — the same doc renders in ~190 ms when driven directly. This document captures the static-analysis findings that point at specific Boss wrappers and the layered bisection rig staged for the next iteration.

## Scope and methodology

- **Static review** of every Boss-side wrapper Textual is rendered inside, plus the Textual sources at the pinned version (gonzalezreal/textual 0.3.1, in `tools/boss/app-macos/.build/checkouts/textual/`).
- **Comparison baseline:** the textual-perf rig from PR #686 (`tools/boss/experiments/textual-perf/`) — same Textual pin, same 46 KB sample, ~190 ms `parse_end`.
- **No live profiler runs** were collected for this writeup; severities are reasoned from the code shape and the difference between the textual-only path and Boss's path. Numbers from the rig in `tools/boss/experiments/textual-perf-layered/` (added in this change) replace these guesses with measurements once a human runs them.
- **What Boss measures as `phase=parse`:** wall clock from `MarkdownViewerScrollContent.onAppear` to the first non-zero height arriving via `StructuredTextHeightKey` (`tools/boss/app-macos/Sources/DesignsView.swift:545-563`). That window covers both the synchronous markdown parse and the first SwiftUI layout pass of the rendered tree. It does _not_ include async attachment resolution. So whatever blows up to 129 s is happening in the synchronous parse or the first layout pass of the Boss wrapper tree.

## The wrappers Boss adds on top of `StructuredText`

The async-markdown-viewer flow (the surface that produced the 129 s number) wraps `StructuredText` in this stack, top-down:

1. `AsyncMarkdownViewerView` switches on `vm.state`. On `.loaded`, builds `MarkdownViewerView(...).id(vm.renderContentID).navigationTitle(title).onAppear { … }` (`tools/boss/app-macos/Sources/DesignsView.swift:619-648`).
2. `MarkdownViewerView` wraps `MarkdownViewerScrollContent(...)` in `.withComments()` (`tools/boss/app-macos/Sources/DesignsView.swift:470-478`).
3. `WithCommentsModifier.body` recomputes `commentedTexts = layer.comments.map(\.quotedText).filter { !$0.isEmpty }` every body pass, wraps the content in an `HStack(spacing: 0)`, injects `.environment(\.commentedTexts, ...)` and `.environment(\.commentFlashText, ...)`, overlays an add-comment button, attaches a hidden ⌘⇧K button via `.background { Button("") {...}.keyboardShortcut(...) }`, and installs/removes NSEvent monitors on appear/disappear (`tools/boss/app-macos/Sources/Comments/CommentLayer.swift:290-345`).
4. `MarkdownViewerScrollContent.body` returns `ScrollView { VStack { Title; Divider; StructuredText(source, parser: markdownParser).bossMarkdown().textual.textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading).id(parseVersion).onChange(of: commentedTexts).onChange(of: commentFlashText).background(GeometryReader for height preference) }.padding(.horizontal, 24).padding(.vertical, 20).frame(maxWidth: .infinity, alignment: .leading) }.textSelection(.enabled).onAppear { parseStartTime = Date() }.onPreferenceChange(StructuredTextHeightKey.self) { … }` (`tools/boss/app-macos/Sources/DesignsView.swift:511-564`).
5. `StructuredText.body` then calls `WithAttachments(attributedString) { BlockContent(content: $0).modifier(TextSelectionInteraction()).modifier(TextSelectionCoordination()) }.coordinateSpace(.textContainer).onChange(of: markup, initial: true) { markupDidChange(markup) }.lineLimit(nil)` (`.build/checkouts/textual/Sources/Textual/StructuredText/StructuredText.swift:118-135`).
6. `bossMarkdown()` swaps in `BossStructuredTextStyle()` whose `codeBlockStyle` wraps each code block in `Overflow { … .padding(12) }.background(RoundedRectangle.fill).overlay(RoundedRectangle.stroke)`, and whose `tableStyle` adds a `Canvas { … }` overlay via `.textual.tableOverlay { layout in Canvas { … for divider in layout.dividers() { … } } }` plus a `RoundedRectangle.stroke` overlay (`tools/boss/app-macos/Sources/BossMarkdownStyle.swift:38-116`).

PR #686's textual-only rig collapses 1–5 down to `ScrollView { StructuredText(markdown: source).padding() }` and runs in ~190 ms. The three-orders-of-magnitude slowdown must come from something added above the `StructuredText(...)` line.

## Hypotheses, ranked

### 1. `.withComments()` HStack-wraps the entire ScrollView, plus a recently-changed observable surface **(HIGH)**

`WithCommentsModifier` is the most recently-added wrapper (`169a9db` on 2026-05-15) and is the only wrapper whose body recomputes a non-trivial value on every `@Published` change of an observable model. Two things make it suspicious:

- It re-creates a fresh `[String]` array via `layer.comments.map(\.quotedText).filter { !$0.isEmpty }` on every body pass. `[String]` is `Equatable`, so SwiftUI's environment-update diff _should_ suppress propagation when the array is empty, but the value still has to be constructed and compared on every observable change.
- It wraps the inner `ScrollView` inside an `HStack(spacing: 0)` and only conditionally adds a sidebar (`if !layer.comments.isEmpty`). SwiftUI's `HStack` propagates a horizontal layout pass; combined with the inner `ScrollView { VStack { … } }`, the inner content is asked to size to a flexible-width column, which then has to interact with `frame(maxWidth: .infinity)` at two levels (on the `StructuredText` and on its enclosing `VStack`).
- The hidden ⌘⇧K shortcut is installed via `.background { Button("") {...}.keyboardShortcut("k", modifiers: [.command, .shift]).frame(width: 0, height: 0).hidden() }`. That's a real `Button` in the view tree with global keyboard-shortcut wiring; SwiftUI re-evaluates it on every body invalidation.

Layer **L3** in the rig isolates this.

### 2. `bossMarkdown()` table style adds a `Canvas { … layout.dividers() … }` overlay **(MEDIUM)**

`BossTableStyle.makeBody` adds `.textual.tableOverlay { layout in Canvas { context, _ in for divider in layout.dividers() { context.fill(Path(divider), with: .style(...)) } } }` (`tools/boss/app-macos/Sources/BossMarkdownStyle.swift:96-105`). The default Textual style does not add a `Canvas` overlay; it uses a different table style. `Canvas` is redrawn whenever the table layout invalidates. Across multiple tables, layout passes can compound — and the editorial-controls doc has several tables.

Layer **L1** in the rig isolates the Boss style stack as a whole. If L1 alone crosses into seconds, this is the prime suspect inside it.

### 3. Boss inner wrappers nest `frame(maxWidth: .infinity)` against `ScrollView` and `textSelection(.enabled)` at two levels **(MEDIUM)**

`MarkdownViewerScrollContent` applies `.frame(maxWidth: .infinity, alignment: .leading)` to both the `StructuredText` and the enclosing `VStack`, and `.textSelection(.enabled)` on the outer `ScrollView` while _also_ applying `.textual.textSelection(.enabled)` on the inner `StructuredText`. The two text-selection modifiers serve different stacks (SwiftUI's native one and Textual's `TextSelectionInteraction` / `TextSelectionCoordination`) and may both stand up the AppKit text-selection plumbing over the same 47 KB body.

Layer **L2** in the rig isolates this.

### 4. `.id(parseVersion)` + `.onChange(of: commentedTexts)` rebuild **(LOW)**

`MarkdownViewerScrollContent` bumps `parseVersion` whenever `commentedTexts` or `commentFlashText` changes, then the `.id(parseVersion)` on `StructuredText` forces SwiftUI to discard the old `StructuredText` and construct a new one from scratch — which triggers a fresh parse. With zero comments the value should never change, so this shouldn't fire on first render. But if there is any spurious environment churn (cf. hypothesis 1), the `.id()` could thrash. This is downstream of hypothesis 1: it can only matter if something upstream is causing the value to change.

The rig captures this indirectly: in L3, the comments stub publishes the same `[String]` values, so any extra rebuilds caused by the `.id` thrash show up there.

### 5. View-model state-machine churn between `.loading` and `.loaded` **(LOW)**

`AsyncMarkdownViewerView` reads `vm.state` directly (not via `@Published` projection) and rebuilds when _any_ `vm.objectWillChange` fires — `renderStartTime`, `pendingRenderProjectShortID`, `renderContentID`, `clickStartTime`, _and_ `state` all live on the same `@MainActor final class AsyncMarkdownViewerViewModel: ObservableObject` (`tools/boss/app-macos/Sources/DesignsView.swift:585-609`). The `.id(vm.renderContentID)` forces a full inner rebuild whenever the UUID changes, which it does once per content load.

Layer **L4** (without async) and **L5** (with async) isolate this together. If L5 is much slower than L4, the spinner→content transition is doing extra work.

### 6. Async fetch dispatch on `Task.detached` `String(contentsOf:)` **(LOW)**

`ChatViewModel.openProjectDesignDoc` dispatches a `Task.detached` to read the file (`tools/boss/app-macos/Sources/ChatViewModel.swift:1089+`). 47 KB is trivial to read; this should not show up. The async hop _itself_ can cause a `.loading` → `.loaded` rebuild as in (5), but the file I/O isn't a bottleneck.

## Bisection plan

`tools/boss/experiments/textual-perf-layered/` re-introduces wrappers 1–4 above on top of `StructuredText`, one per layer, with a picker so a human can click between layers and capture per-layer timing in os.log:

| Layer | What it adds                                                     | Expected (if Boss-side stack only) |
| ----- | ---------------------------------------------------------------- | ---------------------------------- |
| L0    | none (= PR #686)                                                 | ~190 ms                            |
| L1    | `.bossMarkdown()`                                                | hypothesis 2 lights up here        |
| L2    | Boss inner wrappers (frame, dual text-selection, title, padding) | hypothesis 3 lights up here        |
| L3    | `.withComments()` stub                                           | hypothesis 1 lights up here        |
| L4    | view-model `.loading` → `.loaded` flip                           | hypothesis 5 lights up here        |
| L5    | async fetch + view-model                                         | hypothesis 6 lights up here        |

Run:

```sh
cd tools/boss/experiments/textual-perf-layered
swift run textualperflayered
# in another terminal:
log stream --predicate 'subsystem == "com.boss.textualperf"' --level info
```

Each picker click logs `phase=parse_start layer=Ln` and, on first non-zero layout, `phase=parse_end layer=Ln duration_ms=…`. The rig keys its pane by `.id(layer)`, so re-clicking a layer captures a fresh sample.

The expected outcome is that one specific layer crosses from milliseconds into seconds — that's the offender. If two adjacent layers both jump, the wrapper added by the _first_ is the cause (the second is just inheriting the cost).

## L1–L5 bisection result (2026-05-19)

**None of L1–L5 reproduced the wall.** Bisection run 2026-05-19 02:04–02:05Z (rig PID 26591, mono-agent-003, sample doc `tools/boss/docs/designs/installable-distribution-package-for-boss.md`, 47 KB):

All six layers (L0–L5) rendered in human-imperceptible time (~1–3s click-to-click gaps including human reaction time, no layer pauses for tens of seconds). The same wrappers applied in the production Boss app produced a 38-second `phase=render duration_ms=37982` event in `com.boss.app:DesignDocTiming` earlier the same session.

**Caveat:** `parse_end / duration_ms` instrumentation was broken at time of bisection (T635). The "no layer is slow" conclusion is from human-eyeball wall-clock plus click-cadence timestamps, not from numeric durations. Land T635 before interpreting L6+ numbers if hard evidence is needed.

**Implication:** The slowness does not live in any of the Boss-side view wrappers alone. The cause is an interaction between those wrappers and something further up the production scene tree — most likely one of the app-level observables that publish continuously while the window is open.

## L6–L9 hypothesis ladder (extension, added 2026-05-19)

Added layers L6–L9 to `tools/boss/experiments/textual-perf-layered/` to bisect the production scene tree above L5. Each layer adds exactly one production element that is absent from the standalone rig.

### 7. Passive ChatViewModel EnvironmentObject in the tree **(MEDIUM)**

In production, `BossMacApp` injects `ChatViewModel` as an `@EnvironmentObject` on the async-markdown-viewer `Window` scene. The rig (L0–L5) has no such environment injection. `ChatViewModel` has ~50 `@Published` properties; simply being in the environment means every view that declares `@EnvironmentObject var model: ChatViewModel` is subscribed to `model.objectWillChange`. If the design-doc render path reads `chatModel` anywhere (directly or through intermediate views), every publish during the render could trigger body re-evaluation.

Layer **L6** tests this: adds a `ChatViewModelStub` (20 `@Published` vars, no timer) as `@EnvironmentObject`. If L6 is slow, the subscription chain itself is the cost. If L6 matches L5, proceed to L7.

### 8. Sibling publisher firing during render **(HIGH)**

Earlier in the same session that produced the 38 s wall, we observed a kanban resolve spike from ~170 ms → 1,427 ms. This indicates main-thread starvation: the kanban resolver was also trying to run while the markdown window was rendering. The causal arrow may run in both directions — starvation from the kanban resolver delays the render, _and_ objectWillChange publishes from the resolver's `@Published` properties could force the markdown view's body to re-evaluate mid-render.

`ChatViewModel` in production receives engine events continuously — task-runtime updates, worker state changes, live-status probes — every few hundred milliseconds. Any of these fires `objectWillChange` on the shared `chatModel`, which propagates to every subscribed view including any that are mid-layout.

Layer **L7** tests this: `SiblingPublisherStub` fires `objectWillChange` every ~500 ms while L7 is displayed. If L7 is slow and L6 is not, the _active_ publishing cadence (not the EnvironmentObject graph itself) is the culprit.

### 9. NSEvent monitors blocking the event loop **(MEDIUM)**

`CommentLayer.installMonitors()` registers three local monitors: `.keyDown`, `.rightMouseDown`, `.leftMouseUp`. These run on the main thread for every matching event delivered to the app — not just to the markdown window. During a 38-second render, any user mouse movement or accidental key press would fire these closures synchronously on the main thread, potentially adding latency to layout passes that are already running there.

Layer **L8** tests this: installs identical pass-through monitors (no-op handlers) while L8 is displayed, unregisters on disappear. If L8 is slow and L7 is not, the event-monitor overhead is the culprit.

### 10. Combined publish load from all active observers **(MEDIUM)**

Production's `ContentView` has `@StateObject private var workersWorkspace = WorkersWorkspaceModel()` and `@StateObject private var bossPane = BossPaneModel()` alongside `@EnvironmentObject private var model: ChatViewModel`. These three observables each fire independently. The combined objectWillChange cadence may be fast enough to saturate SwiftUI's diffing pass and prevent the layout engine from making forward progress on the markdown tree.

Layer **L9** tests this: adds `ExtraViewModelStub` publishing every ~350 ms on top of L8, approximating the WorkersWorkspaceModel + BossPaneModel combined cadence. If L9 is slow and L8 is not, it is the total combined publish load — not any single publisher — that reproduces the wall.

### Next bisection run

```sh
cd tools/boss/experiments/textual-perf-layered
swift run textualperflayered
# in another terminal:
log stream --predicate 'subsystem == "com.boss.textualperf"' --level info
```

Pick each layer in sequence, L6 → L7 → L8 → L9. Capture 3+ samples per layer. The first layer that crosses from milliseconds into seconds is the offender.

If none of L6–L9 reproduces the wall, the cause is something not captured by the rig — likely full AppDelegate registration (NSApplicationDelegateAdaptor), GhosttyKit terminal views active in the same NSWindow, or a side-effect of the `NavigationSplitView` that wraps `ContentView` in production. File as a follow-up with a description of what L9 measured.

## L6–L9 bisection result + root cause (2026-05-26)

Ran the full L0–L9 bisection live on macOS via a new auto-advance mode in the rig (`TPL_AUTO=1`, which cycles the layers and emits `parse_end duration_ms` without manual picker clicks). **None of L0–L9 reproduces the wall.** All layers render the 47 KB prose sample in ~180–360 ms:

| Layer                      | parse_end (ms, 3 samples) |
| -------------------------- | ------------------------- |
| L0 · Textual only          | 183, 185, 187             |
| L1 · + bossMarkdown()      | 185, 183, 181             |
| L2 · + Boss inner wrappers | 251, 259, 247             |
| L3 · + .withComments()     | 252, 249, 258             |
| L4 · + view-model          | 250, 247, 251             |
| L5 · + async fetch         | 285, 290, 283             |
| L6 · + env object          | 284, 356, 300             |
| L7 · + sibling pub         | 303, 304, 302             |
| L8 · + event monitors      | 329, 304, 302             |
| L9 · + full scaffold       | 293, 294, 301             |

### Two instrumentation problems found and fixed in the rig

1. **The `parse_end` signal silently failed for every wrapped layer (L3+).** It used a `PreferenceKey` (`StructuredTextHeightKey`) that bubbles _up_ the view tree, but `withCommentsStub`'s `HStack` + `.overlay` + `.background` disrupts that bubbling, so L3–L9 never recorded a height and timed out. This is why the earlier (2026-05-19) "L1–L5 fine" conclusion was eyeball-only — `parse_end` was never actually emitted for the wrapped layers. Fixed by switching the height reporter to a _downward-flowing_ environment closure (`reportRenderHeight`), which reaches the reporter regardless of intervening wrappers. After the fix L3–L9 report normally (table above).

2. **The publisher-cadence hypothesis (L7–L9) is mechanically refuted.** `SiblingPublisherStub` is now cadence-configurable (`TPL_PUB_MS`). Cranking it from 500 ms down to **2 ms (~500 publishes/sec)** leaves L6–L9 unchanged at ~225–263 ms. A main-actor publisher cannot slow a main-actor render unless it forces the `StructuredText` to _re-parse_, and with stable `.id`s it does not. So "sibling-publisher invalidation cascade" is not the mechanism.

### Content is the dominant lever (not the scene tree)

A synthetic 46 KB doc heavy on tables/code (≈60 tables + 60 code blocks) renders in **~2.5 s in plain Textual (L0)** — ~13× the prose doc at the same byte size. Boss wrappers add little on top (L0→L3: 2.5 s → 3.1 s; `bossMarkdown` adds ~0 ms). The publisher has no effect even here (L6–L9 ≈ 2.8 s at both 500 ms and 2 ms cadence). So most render cost is intrinsic to Textual's table/code layout of the specific content — but ~3 s still does not reach the production 38 s wall.

### The actual root cause: `phase=render` is a _mount-latency_ window, not a parse window

`phase=render` is logged in `AsyncMarkdownViewerView`'s `.loaded`-branch `.onAppear` relative to `renderStartTime` (stamped just before `state = .loaded` in `ChatViewModel.fetchAndUpdateAsyncMarkdownViewerVM`). It measures the latency from the _state flip_ to the loaded view _mounting_ — it does **not** include the `StructuredText` parse (that is `phase=parse`). The L0–L9 layers all measure parse time, so they were measuring the wrong window.

`AsyncMarkdownViewerView` declared only `@EnvironmentObject var chatModel: ChatViewModel` and read `chatModel.asyncMarkdownViewerVM.state`. But `asyncMarkdownViewerVM` is a _nested_ `ObservableObject` (a plain `let` on `ChatViewModel`, not `@Published`), and the view never subscribed to it. So mutating `asyncMarkdownViewerVM.state` did not re-render the window — the loaded view only mounted on the **next incidental `chatModel` publish** (an engine event). `phase=render` therefore measures "time until the next `chatModel` publish after the content loaded," which under main-thread contention (the observed kanban resolve spike 170 ms → 1,427 ms, bursty engine events) stretches to the tens of seconds seen as the wall. The 129 s → 38 s variability across sessions is consistent with a publish-timing dependence, not a fixed compute cost.

Confirmed in the rig with a dedicated **L10 mount-latency probe** that contrasts the production "buggy" pattern (a view observing a host but reading a nested object's state) against the "fixed" pattern (observe the nested object directly):

| host publish interval (`TPL_PUB_MS`) | buggy mount latency | fixed mount latency |
| ------------------------------------ | ------------------- | ------------------- |
| 500 ms                               | 200–221 ms          | 2–4 ms              |
| 3000 ms                              | 2190–2219 ms        | 2–4 ms              |

Buggy mount latency tracks the host publish interval; fixed is ~3 ms regardless. This is the mechanism, reproduced and measured.

### Fix

`AsyncMarkdownViewerView` now observes `AsyncMarkdownViewerViewModel` **directly** (`@EnvironmentObject var vm: AsyncMarkdownViewerViewModel`, injected on the `async-markdown-viewer` `Window` scene in `BossMacApp`). The loaded view now mounts the instant `state` flips, independent of `chatModel`'s publish timing — eliminating the `phase=render` wall. `chatModel` remains injected on the scene for the comment layer and other descendants.

### Live-app verification still outstanding

The before/after `phase=render` from a live Boss app run could not be captured in this pass: a Boss worker cannot drive the production app (it connects to the engine/DB, which workers must not touch — see `.claude/CLAUDE.md`). The rig L10 probe confirms the mechanism and the fix's effect on mount latency, but a human should still open the editorial-controls doc in the real app before/after this change and paste the `com.boss.app:DesignDocTiming phase=render duration_ms` line into the PR for the record.

### T617 status

T617 (remove the `textual-perf*` rigs) remains **blocked**: the rig is still the only artifact that demonstrates and guards the mount-latency mechanism (L10), and a live before/after confirmation has not yet been recorded. Keep the rig until the fix is verified in the real app.

## Open questions

- Does **L1** alone reproduce the slowness, or is it L2 / L3 that crosses? The static review can't distinguish — only the rig measurements can.
- Does the slowness depend on Designs-tab state (e.g. how many products/projects are loaded) or is it intrinsic to the single doc? The rig is standalone, so if it reproduces, the cause is local.
- Is there a feedback loop between `.withComments()` rebuilding and `parseVersion` bumping? The rig's L3 stub has no NSEvent monitors, which means it captures the rebuild path without the keyboard-event path; if Boss is slower than rig-L3, that gap implicates the monitors or something else only present in Boss.
- **New (L6–L9):** Does the sibling publisher need to be firing at the exact moment the markdown tree first lays out, or does any publish at any time during the ~38 s window reproduce the wall? The rig fires every 500 ms unconditionally; production fires on engine events which may be bursty.
- **New (L6–L9):** Is the slowness only reproducible when the Designs tab is active and the kanban view is also rendering? If so, none of L6–L9 will reproduce it standalone, and the fix must target the interaction between the kanban and the markdown renderer specifically.
