# Boss UI Performance Improvements

- **Date:** 2026-07-25
- **Status:** Design — awaiting review
- **Project:** `proj_18c5b7e300fb2d30_43` (Boss UI Performance improvements)
- **Baseline artifact:** 60 s `sample` of Boss 1.0.369, pid 59513, ARM64 / macOS 26.5.2, captured 2026-07-25 19:58 PDT. Coordinator-private: the sample file itself is not in the repo, so the counts and symbol attributions distilled below — including the ones this doc goes on to correct — are the shared baseline.
- **Remit:** the brief this project was opened from. It lives in Boss rather than in the repo, so its recommendations are reproduced here once, in its own priority order, and referred to by these labels throughout:
  - **P0 #1 — coalesce / debounce work-tree applies.** "Coalesce engine work-tree / work-item events on the main actor (e.g. 16–50 ms debounce, or 'apply latest only' on next run-loop turn)"; prefer diffed updates over "rebuilding the entire published tree every event".
  - **P0 #2 — make `WorkTask` (and board row models) cheaper to diff.** "If `WorkTask` is large, stop using full-struct equality as the invalidation signal" — "prefer equating on a small snapshot (id + status + display fields used by the card)", or a reference type / revision token.
  - **P0 #3 — slim `WorkBoardCardView` layout.** "Split the card into subviews with stable identity", gate expensive badges behind "cheap booleans computed once in the model", lazy-load hover-only chips and dialogs, and reach for `drawingGroup()` "only if profiling shows draw cost".
  - **P1 #1 — invalidate only affected cards.** Map engine events to specific work-item ids; for live status text, update "a narrow observed object per card/slot, not the global work tree".
  - **P1 #2 — terminal pane cost (secondary).** Cap live terminal surfaces, "detach off-screen panes from display link if architecture allows", keep hidden worker panes off full refresh rate.
  - **P2 — keep stall monitoring, use it as a regression signal.** Log stalls during board updates, add dev-only counters, and "treat 'stall spikes when engine floods events' as a CI or manual perf checklist item".
- **Prior art:** [`../investigations/ui-performance-audit-2026-05-07.md`](../investigations/ui-performance-audit-2026-05-07.md) — static audit that predicted this mechanism 11 weeks before the sample confirmed it
- **Related:** [`engine-dispatch-instrumentation.md`](engine-dispatch-instrumentation.md), [`engine-counter-metrics-framework.md`](engine-counter-metrics-framework.md)

## Verdict

Boss burns main-thread CPU because **every kanban card observes the entire `ChatViewModel`**, and that view model has 77 `@Published` properties. Any one of them changing — a transcript chunk, an engine metric, a hover flag, a panel width — invalidates every visible card, which rebuilds a ~35-parameter view whose largest input is a 45-field `WorkTask` struct, and re-runs the layout of a ~340-line card body. Coalescing engine events helps at the margin, but the fan-out is the mechanism. Fix the observation boundary first, then the diff surface, then the layout.

Separately: the main-thread stall watchdog is burning ~7% of a core symbolicating backtraces it then throws away. That is measurement overhead contaminating the artifact used to diagnose everything else, and it is the first thing to fix — not because it stalls the main thread (it does not), but because nothing downstream can be measured cleanly until it stops.

## Goals

1. **Cut Boss.app main-thread CPU under steady engine event load.** The board must keep updating correctly; this is not a fidelity trade.
2. **Stop whole-board invalidation on every small engine update.** A single task's status flip should re-lay-out that card, not the board.
3. **Make the work-item models cheap to diff** at the SwiftUI view-input boundary, where the profile shows the cost actually lands.
4. **Decompose `WorkBoardCardView`** so AttributeGraph can skip unchanged subtrees.
5. **Every change is justified by a before/after 60 s sample** against a stated, repeatable load. "Feels smoother" is not evidence and is not accepted as a completion signal for any task in this plan. Capturing that sample requires a human at the machine — see the explicit handoff rule in "Measurement protocol".

## Non-goals

- **Rewriting the SwiftUI app.** The target is the observation boundary, the diff surface, and the card's view tree — not an architecture migration.
- **Reopening the engine-side JSONL stall detector.** Confirmed fixed: `read_jsonl` and `pending_stalls` each appear **0 times** in the entire 66 MB artifact.
- **Optimising terminal panes as a CPU fix.** The evidence refutes it (see "What the terminals cost" below). Thread-count and footprint growth with N workers is a real concern but a different project.
- **Reducing the ~1.0–1.1 GB footprint.** The project's goal statement is CPU. Memory is called out in the project description but is not scoped here — flagged as an open question rather than silently absorbed.
- **Deleting the stall monitor.** It is the regression signal the plan depends on. It gets made cheap and moved behind a default-off setting, not removed.
- **Spraying `drawingGroup()`.** Layout, not draw, is the cost. Rasterisation is a separate change with its own memory cost and needs its own evidence.

## Evidence

### The window

The sample covers 45284 samples per full-duration thread. The main thread carries 45284 samples, of which **20325 are idle** in `mach_msg2_trap` — so roughly **24950 samples (~55% of the window) are on-CPU main-thread work**.

Inclusive totals, counted at the outermost occurrence per stack so recursion is not double-counted:

| Symbol / family                                      | Inclusive samples | % of window |
| ---------------------------------------------------- | ----------------: | ----------: |
| `AG::Graph` / `AGGraph*` (AttributeGraph)            |             18923 |       41.8% |
| `sizeThatFits` (all layout engines)                  |              5108 |       11.3% |
| `StackLayout.*`                                      |              5053 |       11.2% |
| `placeChildren` / `placeChildren1`                   |              4874 |       10.8% |
| `EngineClient` (1486 main + 2261 own queue)          |              3747 |        8.3% |
| `MainThreadStallMonitor` (2 main + 3164 watchdog)    |              3166 |        7.0% |
| `WorkTask` (any frame)                               |              2760 |        6.1% |
| `ChatViewModel.*`                                    |              2344 |        5.2% |
| `__derived_struct_equals` (all types)                |              2110 |        4.7% |
| `WorkTask.==` (protocol witness)                     |              1991 |        4.4% |
| `outlined init with copy of` / `outlined destroy of` |              1800 |        4.0% |
| `ChatViewModel.applyWorkTree(...)`                   |              1219 |        2.7% |
| `WorkBoardCardView` (body getter + copy/destroy)     |               364 |        0.8% |

The layout families overlap by nesting and must not be summed.

### The mechanism, read off the source

The profile says _what_ is hot. The source says _why_, and the answer is more specific than the remit's problem statement assumed.

**`WorkBoardCardItem` observes the whole view model.** `Sources/WorkBoardCard.swift:45-46` declares `@ObservedObject var model: ChatViewModel` and `@ObservedObject var liveStates: LiveWorkerStateStore` on the per-card view. `ChatViewModel` has **77 `@Published` properties** (`Sources/ChatViewModel.swift`), including `transcriptsByExecutionID`, `automationRunsByID`, `engineMetrics`, `trunkTokenNote`, `bossPanelWidth`, and `editorialActionsByProductID` — none of which any card renders. Every one of them fires `objectWillChange` on every visible card.

The team has already met this. `Sources/ContentView.swift:1030-1038` documents it in a comment: _"the whole-model `@Published` invalidation that hover badges trigger (any card's `onDepBadgeHover`/`onRevisionBadgeHover` re-renders every card in every column)"_. `LazyVStack` was added to bound the damage to on-screen cards. That bounds it; it does not remove it.

**The card is a ~35-parameter view over a 45-field struct.** `WorkBoardCardView` (`Sources/WorkBoardCard.swift:465-643`) takes ~35 stored inputs — `let task: WorkTask` plus `[WorkTask]`, `[WorkDependencyRow]`, `Set<String>`, and several closures. `WorkTask` (`Sources/Models.swift:16-238`) has ~45 stored properties, the majority `String?`. Its body spans lines 649–987: one ~340-line expression.

**This is why `WorkTask.==` is hot, and it is not `applyWorkTree` that makes it hot.** The dominant equality site is SwiftUI change-detection on view _inputs_:

```
579 AGGraphSetOutputValue                                   (AttributeGraph)
579  AG::LayoutDescriptor::compare(...)
539   AGDispatchEquatable
537    protocol witness for static Equatable.== in conformance WorkTask
532     specialized static WorkTask.__derived_struct_equals(_:_:)
```

That is the largest of ~15 such sites; the next five are 261, 151, 130, 104, 98. Those six alone total ~1323 of the 1991 `WorkTask.==` samples. AttributeGraph is comparing a 45-field value type field-by-field, per card, per graph update — because the 45-field value type _is_ the view input. Coalescing the engine-apply path does not touch this number. The remit's first two P0 items are independent of each other, and both are needed.

**`FlowLayout` recomputes everything twice, with no cache.** `Sources/WorkBoardCard.swift:401-463` implements the badge-strip layout with `cache: inout Void`. Both `sizeThatFits` and `placeSubviews` call `computeRows`, and `computeRows` calls `subviews[index].sizeThatFits(.unspecified)` for every chip. SwiftUI may invoke `sizeThatFits` several times per layout pass while resolving a proposal. The `Layout` protocol has `makeCache`/`updateCache` for exactly this; using `Void` opts out. This is a direct, isolated contributor to the 5108-sample `sizeThatFits` family.

**Engine events hop to the main actor one at a time.** `EngineClient.emit` (`Sources/EngineClient.swift:924-928`) is `Task { @MainActor in self.onEvent?(event) }` — one hop per event, no batching. A burst of N engine events is N separate main-actor turns. This matches the 1484 samples under `closure #1 in EngineClient.emit(_:)` on the main thread.

**Cache invalidation is all-or-nothing.** `invalidateWorkCache()` (`Sources/ChatViewModel.swift:2206-2217`) drops ten caches at once — the id index, the dependency and gating prereq graphs, both revision caches, visible items, per-column items, per-column sections, the repo mode, and the ambiguous-repo-name set. Every bucket `didSet` routes through it. One incremental task update discards the whole derived layer, which is then rebuilt lazily on the next read.

**What already exists, and should be extended rather than rebuilt.** This is not virgin ground, and the repo rule is reuse before build:

- `scheduleWorkTreeRefetch` (`Sources/ChatViewModel+EventHandling.swift:605-623`) already debounces invalidation-driven _fetches_ at 150 ms. It coalesces requests, not applies, and does not cover `workItemUpdated`, which goes straight to `applyIncrementalTaskUpdate` synchronously per event.
- `applyIncrementalTaskUpdate` (`Sources/ChatViewModel+WorkItemEvents.swift:235-271`) already avoids full-tree refetch for single-item updates.
- `taskIndexByID`, `cachedGatingPrereqs`, `cachedInReviewRevisionsByParentID`, `cachedItemsByColumn` already make the per-card lookups O(1).
- `.equatable()` on a hand-written `Equatable` row view is **already established in this codebase**, with the same rationale, at `Sources/TranscriptView.swift:174-183`. The card work is applying a known-good local pattern, not inventing one.
- `PopulationTimingLog`, `UISignpost`, `InteractionFrameCounter`, and `StallLog` already exist as instrumentation surfaces with documented near-zero idle cost.

The prior optimisation round fixed the _algorithmic_ costs. What remains is _structural_: who observes what, and how big the diff surface is.

### Prior art: the May audit called this, and the sample confirms it

[`ui-performance-audit-2026-05-07.md`](../investigations/ui-performance-audit-2026-05-07.md) was a static audit of the same app, written without a live process to profile. Reconciling it against this sample is worth doing explicitly, because it tells us which of its calls held and which are still open.

**Confirmed fixed — the sample shows no residual cost, do not reopen:**

- _#3, `TrekIconAssets.image` re-decoding PNGs per render._ A keyed cache with a negative-cache arm now sits at `Sources/TrekIconAssets.swift:65-98`. No image-decode symbol appears anywhere in the sample's hot set.
- _#2, per-pane 0.5 s viewport screen-scrape._ Now gated rather than removed (`reconcileClaudeMonitor`, `Sources/Ghostty/GhosttyTerminalView.swift:1030-1042`), and the gate holds: terminal Boss-binary leaves top out at 180 samples.
- _#6, `visibleWorkItems` / `workSections` recomputing per body._ The `cachedVisibleItems` / `cachedItemsByColumn` / `cachedSectionsByColumn` layer landed.
- _#5's split recommendation,_ partly: `LiveWorkerStateStore` was extracted off `ChatViewModel` as the audit asked.

**Still open, and now measured rather than inferred:**

- _#4, "any consumer of either map invalidates everyone."_ The audit's own note — _"splitting these out so kanban-only views observe a slim object … is the correct refactor"_ — was applied to the live-state store but **not** to the card. `WorkBoardCardItem` still declares `@ObservedObject var model: ChatViewModel`, so the fan-out the audit described simply moved up one level: instead of one store invalidating everyone, 77 `@Published` properties do. That is entry 6 of the breakdown.
- _#9 / #8, per-event `Task { @MainActor }` in `EngineClient.emit`._ Rated LOW in May on the reasoning that each Task is individually cheap. The sample puts 1484 samples under that closure on the main thread, which upgrades it. That is entry 7.

The audit's severity ranking was reasoned from code alone and its own open question asked for exactly this: a live capture to rank the findings. This sample answers it. The two survivors are both observation-boundary problems, which is why this design attacks that boundary first rather than the event rate.

### Finding: the stall monitor is the largest non-idle symbol in the process

```
3183 DispatchQueue_318: boss.diagnostics.stall-watchdog
3164  closure #3 in MainThreadStallMonitor.start()
2715   MainThreadStallMonitor.tick()
2699    specialized static MainThreadBacktrace.symbolicate(_:)
2570     dyld4::APIs::dladdr(void const*, dl_info*)
2552      dyld3::MachOLoaded::findClosestSymbol(...)
```

`findClosestSymbol` is the **#8 top-of-stack leaf process-wide** — above every SwiftUI and AttributeGraph symbol. Symbolication is ~99% of `tick()`.

Sizing it honestly: 3164 samples against a 45284-sample window is **~7% of one core, sustained** — about 12.7% of what the main thread spends on-CPU. It is on a background queue, so it does not stall the main thread. It is still real CPU on a machine that is somebody's laptop, and it is overhead inside the very measurement being used to steer this project.

The cause is visible in `MainThreadStallMonitor.tick()` (`Sources/Diagnostics/MainThreadStallMonitor.swift:169-182`): it symbolicates **all 64 frames** via `dladdr`, and only _then_ asks `isIdleEventLoopStack` whether the capture was a false positive worth discarding. Under a main thread that is ~55% busy, heartbeats routinely land >250 ms late, so this fires constantly — and a large share of those captures are discarded immediately after paying for full symbolication. It is started unconditionally in release at `Sources/BossMacApp.swift:433`.

This is a direct tension with the remit's P2 item ("keep stall monitoring"). **Resolved in review:** keep the monitor in release builds, make its capture path cheap, and put it behind a settings toggle that defaults to **off**. The reviewer's rationale is that UI stalls rarely need debugging, so the default build should pay nothing for the watchdog at all.

Note that the toggle does not make the symbolication fix redundant, and the two are not alternatives:

- The default-off switch removes the cost from every user who is not debugging a stall — which is nearly everyone, nearly always.
- Deferring symbolication is what makes the monitor usable **when it is switched on**. A diagnostic that costs 7% of a core the moment you enable it distorts the very thing you enabled it to observe, and this project's own measurement protocol runs with it enabled.

Gating to debug builds stays rejected: a stall that only reproduces in a release build must remain diagnosable by flipping a setting, not by producing a custom build.

### Finding: the largest Boss-binary leaf is off-main, and not where the baseline distillation placed it

`specialized Collection<>.firstIndex(of:) (in Boss)` at 278 top-of-stack samples is the largest Boss-binary leaf in the process. The distilled baseline placed it inside `applyWorkTree` / `ChatViewModel` and offered it as evidence for the remit's P0 #2. The source says otherwise.

A repo-wide search finds ten `firstIndex(of:)` call sites, and **none of them are on `[WorkTask]`**. The only one on a hot path is `Sources/EngineClient.swift:189`:

```swift
guard let newline = buffer[searchStart...].firstIndex(of: 0x0A) else { ... }
```

That is a byte scan over the socket receive buffer in `consumeLines()`, which the sample independently shows at 940 samples on `DispatchQueue_129: Boss.EngineClient`. The adjacent `buffer.removeSubrange(...newline)` is an O(n) memmove per line on top of it. The `firstIndex(of:)` specialisation for `Data.SubSequence`/`UInt8` is emitted into the Boss binary, which matches the "(in Boss)" attribution.

**This is off the main thread.** If the identification holds, fixing it reduces total process CPU but does **not** advance the stated goal of cutting main-thread CPU. That changes its priority, so the plan treats confirmation as a small investigation task and the fix itself as deferred pending an explicit scope decision (see open questions).

### Finding: engine JSON decode is heavy but off-main

`EngineClient.receiveNext()` is 2114 samples on its own queue, of which `consumeLines()` is 940 and `-[_NSJSONReader parseData:options:error:]` is 939; `newJSONString` is the #24 leaf process-wide at 412. Real cost, wrong thread for this project's goal. Same treatment as above: visible, deferred, promotable if the scope question is answered "total process CPU".

### What the terminals cost

Nothing worth chasing. 149 threads, with named clusters `CVDisplayLink` ×24, `io-reader` ×12, `renderer` ×11, `io` ×11 — about 11 terminal surfaces. Their leaves are dominated by idle waits (`kevent64` 1347056, `__psynch_cvwait` 448127, `poll` 446290 process-wide). The largest Boss-binary terminal leaf is `Renderer.updateFrame` at **180 samples**. For scale, the stall watchdog alone is 17× that. Terminal panes are not the CPU problem, and the plan does not treat them as one.

### Sampling caveat that constrains every comparison

A popup / context menu was open for roughly **5087 main-thread samples** of the window (`-[NSPopUpButtonCell trackMouse:]` → `NSMenuTrackingSession`, 1129 of it idle). That is a nested modal event loop across ~11% of the main thread's window, so the baseline is **not** clean steady state.

Consequence, and it is binding on every task below: **the percentages in this document are directional, not a comparison basis.** No task may claim a win by diffing against these numbers. Each task captures its own controlled before-sample under the protocol below.

## Alternatives considered

### Alternative A — debounce engine events and stop there

The remit's P0 #1 alone: coalesce work-tree applies into a 16–50 ms window, apply-latest-only, and expect layout cost to fall out.

**Rejected as insufficient.** It attacks event _frequency_ while leaving _fan-out per event_ intact. With 77 `@Published` properties feeding every card's `@ObservedObject`, board-wide invalidation is also driven by things a debounce on the work tree never sees — hover state, transcript streaming, live-status pushes, panel geometry. Worse, the profile shows the dominant `WorkTask.==` cost arriving through `AG::LayoutDescriptor::compare`, which is view-input diffing: it is proportional to invalidations × input size, and debouncing engine events reduces neither term for non-work-tree publishes. Debouncing is retained in the plan as a real but second-order component, not as the fix.

### Alternative B — replace `WorkTask` with a class or add a revision token to it

Make `WorkTask` a reference type, or add `revision: UInt64` and equate on that, so `==` becomes a pointer or integer compare.

**Rejected.** `WorkTask` is the app's mirror of `boss_protocol::WorkItem` and is threaded through parsers, the optimistic-kanban override layer, sorting, filtering, dependency resolution, sheets, popovers, and the test suite. Making it a reference type breaks value semantics that the optimistic-move and rollback logic depends on, and introduces aliasing bugs in exactly the code path where correctness is least forgiving. A `revision` token on the model has a subtler failure: it must be bumped on every mutation site or cards silently render stale — a correctness cliff with no compile-time backstop, in a struct that gains fields regularly (the repo's builder-pattern convention exists precisely because this struct keeps growing).

The chosen approach gets the same win — a small, cheap `Equatable` surface — **at the view boundary instead of in the model**, where being wrong causes a redundant redraw rather than a stale card, and where the compiler still checks the mapping. `WorkTask` keeps its value semantics and its derived `==` for the code that legitimately wants a full comparison.

### Alternative C — one `@StateObject` card view model per card

Give each card its own `ObservableObject`, subscribed to a narrow slice of the store.

**Rejected as disproportionate.** It adds N observable objects, N subscriptions, and N object lifetimes to manage against a `LazyVStack` that creates and destroys card views during scrolling — a class of bug (stale subscriptions, retain cycles through the store) that costs more than it saves. A plain `Equatable` value snapshot computed by the parent achieves the same invalidation narrowing with no lifetime management, and matches the pattern already proven in `TranscriptView`.

## Chosen approach

Four phases, in dependency order. Phases 1–3 are the substance; phase 0 exists because nothing can be measured honestly until it lands.

### Phase 0 — make measurement cheap and make the counters exist

**Defer stall-backtrace symbolication.** `capture()` already returns raw addresses and is allocation-free by design. The fix is to keep them raw: store `[UInt]` addresses in `StallRecord`, and symbolicate only when a record is actually read — in `UIStallsViewer` or on export. The idle-stack pre-filter, which today forces full symbolication before it can decide to discard, is replaced by an address-range test: resolve the app image's `[start, end)` bounds **once** at startup, then test each frame numerically. That is an integer comparison per frame instead of a `dladdr` per frame, and it keeps `isIdleEventLoopStack` a pure, unit-testable function over addresses plus a range. Add a floor on symbolication frequency as a backstop.

**Put the monitor behind a default-off setting.** Per review, add a Settings toggle for stall monitoring, defaulting to off, so the common case pays nothing. The monitor remains available in release builds — it is a setting, not a build gate — and the two halves of this item are complementary rather than alternative: the toggle removes the cost for users who are not debugging stalls, and the deferred symbolication is what makes the monitor affordable for the ones who are, including every capture run under the measurement protocol below. The remit's P2 is preserved: stall monitoring survives and is one toggle away.

**Add dev-visible UI counters** on the existing `PopulationTimingLog` / `os_signpost` rails rather than a new subsystem: `applyWorkTree` calls/sec, incremental task updates/sec, engine events delivered to the main actor/sec, and card body evaluations/sec. Atomic increments, flushed on a 1 Hz timer, zero cost when nothing increments. These are the per-task regression signal — a sample tells you where time went, a counter tells you whether the fan-out actually narrowed.

### Phase 1 — narrow the observation boundary (the main event)

**Introduce `WorkCardSnapshot`**: a small `Equatable` value type holding exactly the fields the card renders, plus booleans that are today recomputed inside `body` (`isDispatchPending`, `isResolvingConflicts`, `isRemediatingCI`, `isAIReviewing`, badge visibility). It is built from a `WorkTask` plus the resolved per-card context. Two `WorkTask`s that differ only in fields the card does not render must produce equal snapshots — that property is the whole point and is what the unit tests assert.

**Make `WorkBoardCardView` consume the snapshot, conform to `Equatable`, and apply `.equatable()`** at the call site, exactly as `SegmentRowView` does. The AttributeGraph compare surface becomes a small struct instead of a 45-field one. This is the direct fix for the `LayoutDescriptor::compare` → `WorkTask.==` path.

**Drop `@ObservedObject` from the card.** `WorkBoardCardItem` keeps a non-observing reference to the model for action dispatch (`selectWorkCard`, drag handlers) and receives its snapshot as a value. The column container already observes the model; it recomputes snapshots for its cards, and `.equatable()` stops the cards whose snapshot is unchanged before body evaluation or layout. Same treatment for `liveStates` — the resolved live-status value is passed in rather than the store being observed per card.

This is the change that turns "77 unrelated publishes invalidate every card" into "a card re-renders when its own inputs change".

### Phase 2 — coalesce, and make invalidation targeted

**Batch engine→UI delivery.** Replace the per-event `Task { @MainActor }` in `EngineClient.emit` with an accumulate-and-drain: events enqueue, and one main-actor task drains the queue on the next turn. A burst of N events becomes one main-actor transaction and one SwiftUI update, rather than N. Ordering is preserved; the existing 150 ms `scheduleWorkTreeRefetch` debounce stays and is not duplicated.

**Make `invalidateWorkCache` keyed.** A single-item update should drop the caches that item can affect, not all ten. The id index can be patched in place for an update that changes no bucket membership; the dependency and revision caches only need dropping when edges or revision rows actually change.

### Phase 3 — cut the layout cost

**Give `FlowLayout` a real cache** — `makeCache`/`updateCache` holding the computed rows and per-subview sizes, so `placeSubviews` reuses what `sizeThatFits` computed instead of redoing every chip measurement.

**Decompose `WorkBoardCardView.body`** into stable subviews — revision header, title row, live-status row, badge strip, footer/PR row — each `Equatable` over its own slice of the snapshot, so AttributeGraph skips subtrees whose inputs did not move. A status flip should re-lay-out the status row, not the card.

**Make secondary UI lazy** — hover-only chips, popovers, and confirmation dialogs should not be constructed for idle cards.

### A note on what "success" will look like in the sample

`WorkBoardCardView`'s own frames are only 364 samples. Decomposing the card will **not** show up as a drop in symbols named `WorkBoardCardView`. The body getter is cheap; what is expensive is the view tree it returns and the layout of that tree. The win appears in `sizeThatFits`, `StackLayout`, `placeChildren`, and the `AG::Graph` family. Any task that reports "no change in `WorkBoardCardView` samples, therefore no win" has measured the wrong symbol.

## Measurement protocol

Binding on every task in the breakdown. A task that cannot state its before/after numbers is not done.

### Who runs this: a human, not the implementing agent

**Every step in this protocol is a human step. An agent implementing any task in this plan must not attempt it.**

Boss is a macOS GUI app on somebody's laptop. Agent workers are explicitly forbidden from launching it — however the launch is spelled (`open`, the bundle executable, an unpacked copy, `bazel run` of an `app-macos` target) — because doing so puts a window on a person's screen mid-work and terminates the engine they are actually using. Agents also have no way to establish the load definition below (a populated board, ≥3 live workers, a frontmost window, a still pointer), and no way to observe the result. This is stated here in full rather than left implicit, so no agent is dispatched at a task it is not permitted to perform.

Consequently, for every entry in the breakdown whose gate is a sample:

- **The agent's deliverable is the code change plus a capture request.** The change lands with its unit tests and functional checks green, and the PR states: the exact load to establish, the symbols and counters to read, which of them the change was expected to move, and in which direction. Point 5 of "Report, per task" — the prediction written before the after-sample — is the agent's, and writing it down beforehand is what makes the human's number a test rather than a rationalisation.
- **The human runs the before/after capture and returns the numbers**, which are then recorded against the task. A perf task is not closed on the agent's say-so; it is closed when the returned numbers are on it.
- **`bazel build` and `bazel test` are unaffected by any of this** and remain fully the agent's responsibility. Only the sampling and the on-screen functional sweep need a person.
- **An agent that cannot get the numbers must say so and stop**, not substitute reasoning about why the change should be faster. "The change looks correct" is not a measurement, and this plan does not accept it as one.

Entries 1, 2, 3, 5, 6, 7, 8, 9, 10, 11 and 16 all depend on a human capture. Entry 4 is the only in-scope entry that does not — it ships no behaviour change and gates entirely on unit tests, which is why it is the one place an agent can self-certify. Entries 12, 13, 14 and 15 are deferred and unscheduled; of those, only entry 15 has a gate that is not a `sample` at all — it is a CI signal, and it is the eventual way out of this whole dependency.

### The protocol itself

**Load definition.** Boss open on the kanban, populated board for the `boss` product, all columns visible, ≥3 live workers running, window frontmost, **no menu or popover open** (the baseline artifact's ~11% modal-loop contamination must not be reproduced), no scrolling or pointer movement during capture.

**Stall monitoring on.** After entry 1 lands, stall monitoring defaults to off. Capture runs turn it **on** in Settings — its log is the regression signal several entries report against — and both the before- and after-sample of a given task must agree on the setting. A comparison with the monitor on for one half and off for the other is void.

This rule binds entries 2–16. **Entry 1 is exempt, and has to be:** the on-vs-off delta _is_ its result, and its before-sample predates the setting entirely, because until entry 1 lands the monitor is unconditionally on with no way to turn it off. Entry 1 therefore reports three readings — the pre-change baseline, post-change with the monitor on, and post-change with it off — and is the only entry permitted to compare across settings.

**Capture.**

```sh
sample <Boss_pid> 60 -file /tmp/boss-before-<task>.txt
# land the change, relaunch, re-establish the same load
sample <Boss_pid> 60 -file /tmp/boss-after-<task>.txt
```

**Report, per task.** Every performance task states all five:

1. Process %CPU, mean over the window.
2. Main-thread on-CPU samples — total main-thread samples minus `mach_msg2_trap` idle.
3. Inclusive samples for the fixed symbol list: `AG::Graph`, `sizeThatFits`, `StackLayout`, `placeChildren`, `WorkTask.==`, `__derived_struct_equals`, `applyWorkTree`, `MainThreadStallMonitor`.
4. The Phase 0 counters over the same window: applies/sec, incremental updates/sec, main-actor deliveries/sec, card body evaluations/sec.
5. A one-line statement of which of the above the change was _expected_ to move, written before the after-sample is taken.

**Baseline discipline.** The tables in this document are the shared starting picture, not a comparison basis — the modal-loop contamination makes them unsuitable for that. Each task captures its own before-sample. Where a task lands on top of an earlier one, its before-sample is taken on the earlier task's merged state.

**Functional gate**, on every card-touching task: card status, badges, PR links, revision rollups, blocked chips, merge-queue badges, dependency highlighting, and filters all still correct; `bazel test //tools/boss/app-macos/...` green. No new main-thread I/O or synchronous engine RPCs on the UI path. The `bazel test` half is the agent's; the on-screen visual sweep is a human step for the same reason the sampling is, and the agent's PR should list exactly what to look at.

## Risks / open questions

**The snapshot can go stale by omission.** If a field is added to `WorkTask` and rendered by the card but not added to `WorkCardSnapshot`, the card renders stale data — and it fails silently. Mitigation: the snapshot is built in one function with an exhaustive `switch`-free explicit field list, unit-tested against "same snapshot ⇒ same rendered inputs", and the builder lives next to the card so the diff that adds a rendered field touches it. This is a real ongoing maintenance cost and reviewers should weigh it against the win. It is the same trade `SegmentRowView` already took.

**Phase 1 moves work from the cards to the container.** After the change, the column container recomputes N snapshots per model publish even when nothing rendered changes. Snapshot construction must stay genuinely cheap or the fan-out is merely relocated. The Phase 0 counters plus the sample are what confirm which happened; if snapshot construction shows up hot, the answer is to make the container's recompute incremental (only for ids the update touched), which is a follow-up, not a redesign.

**`.equatable()` is a correctness hazard if the conformance is wrong.** An `==` that returns `true` when a rendered input changed produces a card that never updates. The conformance must be derived from the snapshot alone, never hand-written per field.

**Detaching `@ObservedObject` must not detach action dispatch.** The card still calls into the model. Holding a non-observing reference is intended and safe, but any code path that reads mutable model state _during_ body evaluation must be moved into the snapshot or it becomes an unobserved read — a stale-render bug that will not reproduce reliably.

**Terminal thread growth is unaddressed.** ~11 surfaces, 24 `CVDisplayLink` threads, 1.0–1.1 GB footprint. Out of scope here on the evidence, but the footprint number in the project description is not explained by anything in this plan.

**Measurement is gated on human availability.** Every before/after number this plan depends on has to be captured by a person at the machine (see "Who runs this"). That is a real scheduling dependency, not a formality: an agent can land entry 5 in an afternoon and the task still cannot close until someone runs two 60-second samples. Batching captures — landing several independent entries and sampling once per batch — is tempting and is wrong for anything on the serialised `WorkBoardCard.swift` chain, where per-entry attribution is the whole point. It is acceptable for the depth-0 entries, which touch disjoint files.

**Open questions for a human.** The still-open ones — 2 and 3 — are also filed as a questions manifest alongside this doc (`boss-ui-performance-improvements.attentions.json`), which the engine reads to raise them inline in the design-doc viewer. Question 1 is answered and is therefore **not** in the manifest; it is kept here, struck through, only so the numbering that the entries below cite stays stable.

1. ~~**Stall monitor disposition.**~~ **Answered in review.** Keep the monitor in release, defer symbolication so it is cheap, and add a Settings toggle that **defaults to off** — the reviewer's reasoning being that UI stalls rarely need debugging, so the default build should not pay for the watchdog. Debug-build gating remains rejected. Folded into entry 1; no longer blocking Phase 0.
2. **Is off-main CPU in scope?** The goal says main-thread CPU. The `consumeLines` buffer-scan and engine JSON-decode findings above are both real and both off-main, and are currently tagged deferred on that basis (entries 12 and 13). If the real concern is the process sitting at 25–90% CPU overall, they should be promoted.
3. **Is the 1.0–1.1 GB footprint in scope?** Named in the project description, absent from the goal, and not addressed by anything here.

## Proposed implementation task breakdown

Sixteen entries. Dependency depth and parallelism are called out per entry; file-overlap serialisation is called out where it is real.

**Read the measurement protocol's "Who runs this" section before scheduling any of these.** Where an entry's **Metric** names sample counts, that metric is captured by a human, not by the agent implementing the entry — agents must not launch Boss.app. The implementing agent lands the code, keeps `bazel build` / `bazel test` green, and writes down which numbers the change should move and in which direction; a person captures the before/after pair and returns them. Entries whose metric needs no capture say so explicitly.

**Land entry 1 first**, ahead of everything else. That is a measurement precondition, not a dependency: nothing needs entry 1 to compile, and it has no dependents anywhere in the ladder below. But the watchdog is ~7% of a core inside every sample this plan reads, so any before-sample captured while it is still symbolicating eagerly is contaminated before it is taken.

**The critical path proper is the serialised `WorkBoardCard.swift` chain**: 3 (`FlowLayout` cache) and 4 (`WorkCardSnapshot`) → 5 (card consumes snapshot) → 6 (detach observation) → 9 (decompose body) → 10 (lazy secondary UI). Five entries deep, depths 0 through 4, and it carries the plan's headline win. Entries 3 and 4 sit concurrently at its head for different reasons — 3 because it must vacate `WorkBoardCard.swift` before entry 5 rewrites it, 4 because it is entry 5's hard dependency. Everything else (2 → 7 → 8, plus 11) runs alongside.

---

### 1. Defer stall-backtrace symbolication and put the monitor behind a default-off setting

Two complementary halves, both from the review decision on open question 1.

**Make the capture cheap.** Stop `MainThreadStallMonitor.tick()` symbolicating 64 frames through `dladdr` before deciding whether to discard the capture. Store raw `[UInt]` addresses on `StallRecord`; symbolicate lazily in `UIStallsViewer` and on export. Replace the idle-stack pre-filter's per-frame `dladdr` with an app-image address-range test, resolving the image bounds once at startup, keeping `isIdleEventLoopStack` a pure function over addresses plus a range. Add a floor on symbolication frequency as a backstop.

**Make it opt-in.** Add a stall-monitoring toggle in Settings, defaulting to **off**, and start the monitor from that setting rather than unconditionally at `Sources/BossMacApp.swift:433`. Toggling it must start and stop the monitor live, without a relaunch — a developer noticing a stall should be able to switch it on and catch the next one. When off, the watchdog queue must not exist and no timer may run: "off" means zero cost, not a cheap tick. The monitor stays available in release builds; debug-only gating is rejected.

- **Effort:** medium
- **Dependencies:** none
- **Files:** `Sources/Diagnostics/MainThreadBacktrace.swift`, `Sources/Diagnostics/MainThreadStallMonitor.swift`, `Sources/Diagnostics/StallLog.swift`, `Sources/Diagnostics/UIStallsViewer.swift`, `Sources/BossMacApp.swift`, plus the settings view and its defaults store
- **Metric — human capture required** (see "Who runs this"): three readings, per the exemption this entry holds from the "both samples agree on the setting" rule. **Before**, on the unmodified build where the monitor is unconditionally on: `MainThreadStallMonitor` inclusive samples and `dyld3::MachOLoaded::findClosestSymbol` top-of-stack (document baselines 3166 — ~7% of window — and 2552, for orientation only; the entry's own before-sample is what it reports against). **After, monitor on:** target both under 300. **After, monitor off** (the new default): both must be **zero**, and that is the most important of the three. The agent-side gate is unit tests over `isIdleEventLoopStack` as a pure address/range function, and that flipping the setting starts and stops the monitor.
- **Functional check (human):** recorded stalls still appear in the UI Stalls window with correct symbols after the change, with the setting on.
- Scope: in-scope

### 2. Add dev-visible UI update counters

Add counters on the existing `PopulationTimingLog` / `os_signpost` rails: `applyWorkTree` calls/sec, incremental task updates/sec, engine events delivered to the main actor/sec, card body evaluations/sec. Atomic increments flushed on a 1 Hz timer; no cost when nothing increments. This is the per-task regression signal for everything downstream and must exist before the Phase 1 work so its effect is readable as a fan-out number, not only as a sample delta.

- **Effort:** small
- **Dependencies:** none — runs in parallel with entry 1
- **Files:** `Sources/Diagnostics/PopulationTimingLog.swift` (or a sibling), `Sources/ChatViewModel+WorkItemEvents.swift`, `Sources/EngineClient.swift`
- **Metric — human capture required:** counters emit under load and read zero at idle; overhead below noise in a 60 s sample (no new symbol above 50 inclusive samples). The agent-side gate is unit tests over the counter accumulation and flush logic; only the on-device reading and the overhead confirmation need a person.
- Scope: in-scope

### 3. Give `FlowLayout` a layout cache

Replace `cache: inout Void` with a real `makeCache`/`updateCache` holding computed rows and per-subview sizes, so `placeSubviews` reuses what `sizeThatFits` computed instead of re-measuring every chip. Extract `FlowLayout` from `WorkBoardCard.swift` into its own file as part of the change, which also removes it from the file that entries 5, 6 and 9 rewrite.

- **Effort:** small
- **Dependencies:** none — runs in parallel with entries 1 and 2
- **Files:** new `Sources/FlowLayout.swift`, `Sources/WorkBoardCard.swift` (deletion only)
- **Ordering note:** lands before entries 5, 6 and 9, which rewrite `WorkBoardCard.swift` substantially. Landing it first keeps the deletion trivial to rebase.
- **Metric — human capture required:** `sizeThatFits` inclusive samples, baseline 5108; `placeChildren` baseline 4874. Expected to move both; report each.
- Scope: in-scope

### 4. Introduce `WorkCardSnapshot`

Add a small `Equatable` value type holding exactly the fields `WorkBoardCardView` renders, plus the booleans currently recomputed inside `body` (`isDispatchPending`, `isResolvingConflicts`, `isRemediatingCI`, `isAIReviewing`, per-badge visibility). Add the builder that constructs it from a `WorkTask` plus resolved per-card context. Model and tests only — no view changes, so it lands independently and reviewably.

- **Effort:** medium
- **Dependencies:** none — runs in parallel with entries 1, 2 and 3
- **Files:** new `Sources/Models+WorkCardSnapshot.swift`, `Tests/BossTests/`
- **Metric — no capture needed:** no runtime metric; this entry ships no behaviour change. Gate is unit tests asserting that two `WorkTask`s differing only in non-rendered fields produce equal snapshots, and that every rendered field is covered. The only in-scope entry an implementing agent can close on its own.
- Scope: in-scope

### 5. `WorkBoardCardView` consumes the snapshot and conforms to `Equatable`

Change the card's inputs from ~35 loose parameters (including the 45-field `WorkTask`) to the snapshot plus its closures, derive `Equatable` from the snapshot, and apply `.equatable()` at the call site — the pattern already used by `SegmentRowView` at `Sources/TranscriptView.swift:174-183`. This is the direct fix for the `AG::LayoutDescriptor::compare` → `WorkTask.==` path.

- **Effort:** large
- **Dependencies:** entry 4 (`WorkCardSnapshot`); lands after entry 3
- **Files:** `Sources/WorkBoardCard.swift`, `Sources/ContentView.swift`
- **Metric — human capture required:** `WorkTask.==` inclusive samples, baseline 1991; `__derived_struct_equals` baseline 2110; `outlined init with copy of` / `outlined destroy of` baseline 1800. Plus card body evaluations/sec from entry 2.
- Scope: in-scope

### 6. Detach the card from whole-`ChatViewModel` observation

Remove `@ObservedObject var model: ChatViewModel` and `@ObservedObject var liveStates: LiveWorkerStateStore` from `WorkBoardCardItem`. Keep a non-observing model reference for action dispatch only; the column container computes each card's snapshot and resolved live-status value and passes them as values. This converts "any of 77 `@Published` properties invalidates every visible card" into "a card re-renders when its own inputs change", and is the entry the rest of the plan's headline win depends on.

- **Effort:** large
- **Dependencies:** entry 5
- **Files:** `Sources/WorkBoardCard.swift`, `Sources/ContentView.swift`
- **Ordering note:** substantial co-edit with entry 5 in both files. Must land after it and forward-port entry 5's changes preservingly — integrate, never revert.
- **Metric — human capture required:** card body evaluations/sec from entry 2 is the primary signal — expect a large drop under a load that publishes non-board state (streaming transcript, engine metrics). Plus `AG::Graph` inclusive, baseline 18923, and main-thread on-CPU samples, baseline ~24950. This entry carries the plan's headline win, so its capture is the one least worth batching with anything else.
- Scope: in-scope

### 7. Batch engine→UI event delivery

Replace the per-event `Task { @MainActor in ... }` in `EngineClient.emit` with an accumulate-and-drain: events enqueue and one main-actor task drains the queue on the next run-loop turn, preserving order. A burst of N engine events becomes one main-actor transaction. Do not duplicate the existing 150 ms `scheduleWorkTreeRefetch` debounce — it stays and covers a different boundary (fetch requests, not deliveries).

- **Effort:** medium
- **Dependencies:** entry 2 (needs the deliveries/sec counter to show the batching worked)
- **Files:** `Sources/EngineClient.swift`, `Sources/ChatViewModel+EventHandling.swift`
- **Parallelism:** independent of entries 3–6; may run alongside them
- **Metric — human capture required:** main-actor deliveries/sec from entry 2; `EngineClient` main-thread inclusive samples, baseline 1486 of 3747 total.
- Scope: in-scope

### 8. Make work-cache invalidation targeted

Replace the blanket `invalidateWorkCache()` — which drops ten caches on every bucket `didSet` — with keyed invalidation. A single-item update patches `taskIndexByID` in place when bucket membership is unchanged, and drops the dependency and revision caches only when edges or revision rows actually change.

- **Effort:** medium
- **Dependencies:** entry 7
- **Files:** `Sources/ChatViewModel.swift`, `Sources/ChatViewModel+WorkItemEvents.swift`, `Sources/ChatViewModel+BoardHelpers.swift`
- **Metric — human capture required:** `ChatViewModel.*` inclusive samples, baseline 2344; `applyWorkTree` baseline 1219. Correctness gate is the agent's and is explicit: a task update that changes project membership, kind, or a dependency edge must still fully invalidate — cover each in unit tests.
- Scope: in-scope

### 9. Decompose `WorkBoardCardView.body` into stable subviews

Split the ~340-line body (`Sources/WorkBoardCard.swift:649-987`) into revision header, title row, live-status row, badge strip, and footer/PR row, each an `Equatable` view over its own slice of the snapshot, so AttributeGraph can skip subtrees whose inputs did not move. A status flip should re-lay-out the status row, not the whole card.

- **Effort:** large
- **Dependencies:** entry 6
- **Files:** `Sources/WorkBoardCard.swift`, new sibling files per subview
- **Ordering note:** rewrites the same file as entries 5 and 6; strictly after both.
- **Metric — human capture required:** `sizeThatFits` baseline 5108, `StackLayout` baseline 5053, `placeChildren` baseline 4874, `AG::Graph` baseline 18923. Note explicitly: `WorkBoardCardView`'s own samples (364) are **not** the success metric — the body getter is cheap, the tree it returns is not.
- Scope: in-scope

### 10. Make secondary card UI lazy

Stop constructing hover-only chips, popovers, and confirmation dialogs for idle cards; build them on demand when hover or presentation state actually flips. `WorkCardPopover.swift` is ~700 lines of view code currently reachable from every card's body.

- **Effort:** medium
- **Dependencies:** entry 9
- **Files:** `Sources/WorkBoardCard.swift`, `Sources/WorkCardPopover.swift`
- **Metric — human capture required:** `AG::Graph` inclusive and card body evaluations/sec, measured on an idle board with no pointer over any card. The "no pointer over any card" condition is itself a reason this cannot be automated or agent-run.
- Scope: in-scope

### 11. Investigate: attribute the `firstIndex(of:)` hot leaf

Confirm or refute that `specialized Collection<>.firstIndex(of:) (in Boss)` — 278 top-of-stack samples, the largest Boss-binary leaf in the process — is the byte scan at `Sources/EngineClient.swift:189` in `consumeLines()`, and record which thread it runs on. Static analysis says yes and says it is off-main, which contradicts the distilled baseline's expectation that it lives in `applyWorkTree`. Output is a short investigation note under `tools/boss/docs/investigations/` plus a recommendation on whether entry 12 should be promoted out of deferred. Investigation only — no fix.

- **Effort:** small
- **Dependencies:** none — runs in parallel with everything
- **Files:** new `tools/boss/docs/investigations/` note
- **Metric — human capture required:** a targeted sample with the leaf attributed to a named thread and call site; no code change to measure. Note the shape of this entry: an agent can do the static half — enumerate the call sites, read `consumeLines()`, and write the note's argument — but the sample that confirms or refutes it has to come from a person. Split it that way rather than dispatching an agent at the whole thing; the static half already exists in the "Findings" section above and only needs the capture to close.
- Scope: in-scope

### 12. Reduce the `EngineClient` receive-buffer scan cost

Replace the repeated `firstIndex(of: 0x0A)` scan plus O(n) `removeSubrange` per line in `consumeLines()` with an index-advancing reader that does not re-copy the buffer per line.

- **Effort:** small
- **Dependencies:** entry 11
- **Metric:** `firstIndex(of:)` top-of-stack, baseline 278; `consumeLines()` inclusive, baseline 940 on `DispatchQueue_129`.
- Scope: deferred (future / not a v1 blocker) — off the main thread, so it does not advance the stated goal of cutting main-thread CPU. Promote if open question 2 is answered "total process CPU".

### 13. Reduce engine event JSON decode volume

Cut `-[_NSJSONReader parseData:options:error:]` cost (939 samples) and `newJSONString` (412, #24 leaf process-wide) by sending fewer, larger engine events or by decoding incrementally. Spans the engine and the app and would need splitting into per-subsystem entries before scheduling.

- **Effort:** large
- **Dependencies:** entry 7
- **Metric:** `EngineClient.receiveNext()` inclusive, baseline 2114 on its own queue.
- Scope: deferred (future / not a v1 blocker) — off-main, and multi-subsystem. Promote only with open question 2 answered "total process CPU", and split into engine-side and app-side entries at that point.

### 14. Cap live terminal surfaces and display-link rate

Detach off-screen terminal panes from their display link and ensure hidden worker panes do not run at full refresh rate. This is the remit's P1 #2, which the remit itself flags as secondary.

- **Effort:** medium
- **Dependencies:** none
- **Metric:** thread count and physical footprint as a function of N workers, baselines 149 threads and 1.0 GB (1.1 GB peak).
- Scope: deferred (future / not a v1 blocker) — the evidence refutes terminals as a CPU cause: the largest Boss-binary terminal leaf is `Renderer.updateFrame` at 180 samples, against 3164 for the stall watchdog alone. This is a footprint and thread-count concern, which is a different project.

### 15. Perf regression gate in CI

Make "main-thread stall spikes when the engine floods events" a checked signal rather than a manual step, using the entry 2 counters and the entry 1 stall log. This is the automation half of the remit's P2 — "treat 'stall spikes when engine floods events' as a CI or manual perf checklist item" — the half that would replace the manual checklist with a machine-checked one.

- **Effort:** large
- **Dependencies:** entries 1, 2, 16
- **Metric — no `sample` capture:** gate reproducibly fails on a seeded regression and passes on `main`, which is a CI signal rather than a human reading. The one human step it does need is validating that seeded regression the first time, so that a green gate means something. The harness must also enable stall monitoring explicitly — after entry 1 it is off by default, so a gate that reads the stall log without turning it on measures nothing and passes vacuously.
- Scope: deferred (future / not a v1 blocker) — needs a reproducible synthetic engine-event load harness that does not exist yet, and CI runners cannot capture `sample` output of a GUI app under controlled load. That last constraint is the same one that makes every entry above depend on a human capture; this entry is the eventual way out of it, which is also why it is the hardest one here. Revisit once the manual protocol has a few cycles of data behind it.

### 16. Before/after verification sweep and report

Capture the full before/after comparison across the landed work under one controlled load per the measurement protocol, and write the result up as a doc PR: process %CPU, main-thread on-CPU samples, the fixed symbol list, and the entry 2 counters. Confirms the hot stacks moved away from continuous `sizeThatFits` and `WorkTask` equality, records what did **not** move, and runs the functional checklist (card status, badges, PR links, revision rollups, blocked chips, merge-queue badges, dependency highlighting, filters). This is the project's acceptance evidence and must not be folded into any implementing entry — the implementer measuring their own change is per-task discipline; this is the independent whole-project number.

**This entry is human work end to end and must not be dispatched to an agent as an implementation task.** It is nothing but capture and on-screen verification: two samples under a controlled load, plus a visual sweep of the board. An agent can help afterwards by turning the returned numbers and the raw `sample` files into the write-up, and that is a reasonable way to split it — but the agent must be handed the artifacts, never asked to produce them.

- **Effort:** medium (dominated by the human's time at the machine, not by the write-up)
- **Dependencies:** entries 1, 2, 3, 5, 6, 7, 8, 9, 10 — every in-scope entry that lands a behaviour change, including entry 2, whose counters this entry reports. Entry 4 comes along transitively through 5; entry 11 is an investigation with no landed change to measure.
- **Files:** new `tools/boss/docs/investigations/` or `postmortems/` report
- **Metric:** the full protocol, against a clean before-sample captured with no menu or popover open — explicitly **not** against the numbers in this document, which carry ~11% modal-loop contamination. Both samples with stall monitoring in the same state; record which.
- Scope: in-scope

---

**Parallelism summary.**

- **Depth 0**, all concurrent: 1 (stall monitor — cheap capture plus the default-off toggle), 2 (counters), 3 (`FlowLayout` cache), 4 (`WorkCardSnapshot`), 11 (`firstIndex` investigation), 14 (terminals, deferred).
- **Depth 1:** 5 (card consumes snapshot, needs 4) and 7 (event batching, needs 2) run concurrently — different files, no overlap.
- **Depth 2:** 6 (detach observation, needs 5) and 8 (targeted invalidation, needs 7) run concurrently.
- **Depth 3:** 9 (decompose body, needs 6).
- **Depth 4:** 10 (lazy secondary UI, needs 9).
- **Serialised on `WorkBoardCard.swift`:** 3 → 5 → 6 → 9 → 10. These five substantially co-edit that file and must land in that order, each forward-porting its predecessors preservingly.
- **Final:** 16 (verification sweep), after every in-scope entry that lands a behaviour change — 1, 2, 3, 5, 6, 7, 8, 9, 10.
- **Off the ladder:** 12, 13 and 15 are deferred and unscheduled (14 is deferred too, but is listed at depth 0 above because it depends on nothing). 15 would come after 16, since it needs the sweep's numbers to calibrate its threshold.
