# Boss macOS UI: performance fixes

Companion to the 2026-05-07 audit
(`tools/boss/docs/investigations/ui-performance-audit-2026-05-07.md`).
The audit enumerates nine findings and recommends nine
independently-mergeable chores. This doc decides the structural
calls the audit deferred, defines the phasing, and lists the
acceptance criteria each landing chore is held to.

The reported symptom is "the Boss UI gets sluggish after long
sessions." The audit splits that into two distinct curves:

- **Steady-state main-thread cost.** Per-pane 0.5 s screen
scrapes (#2), PNG decodes on every render (#3), full kanban
recomputes on every engine event (#6), and full
`ContentView` invalidation on every `worker.live_states`
push (#4). These do not grow with session length but
multiply per event, so they get worse as more workers
spawn and as engine event rates climb.
- **Per-event allocation pressure / unbounded growth.**
libghostty surfaces never freed (#1), Boss assistant chunks
appended to a `timeline` no view reads (#5, #8), terminal
entry index dictionary that only grows (#7), and one
`MainActor` `Task` per parsed engine event (#9). These
*do* grow with session length and account for the long-tail
sluggishness.

Each fix is small. The structural decisions are about how
they fit together so we don't re-paper-cut the same surfaces
across multiple PRs.

## Goals

- RSS for a multi-hour Boss session is bounded by the active
working set, not by total events received.
- SwiftUI body re-evaluation rate on `ContentView` is
proportional to UI-meaningful changes, not engine event
rate.
- Each fix lands as one PR with a clear acceptance signal,
so a future regression is bisectable.
- No behavior visible to humans changes (status pill, kanban
dot, idle copy, slot allocation) — these are
implementation-only fixes.

## Non-Goals

- Replacing the screen-scrape Claude monitor with something
new. The right long-term move is "delete it once
`LiveWorkerState` is the source of truth"; the audit's
recommendation is to gate it, not redesign it.
- Re-architecting `ChatViewModel`. The split of
`workerLiveStates*` into a child store is a local refactor,
not a rewrite. A broader split (per-product child stores,
etc.) is out of scope.
- Adding instrumentation. The acceptance signals below are
intentionally things a human can verify with
`sample` / `leaks` / `Activity Monitor` rather than new
in-app counters. We add instrumentation only if a fix
needs it for validation.
- Cross-platform. iOS / iPad surfaces don't exist; this is
all `tools/boss/app-macos/Sources/`.

---

## Design Question 1 — libghostty surface teardown order

### Problem

`GhosttyTerminalHostView.deinit` (`GhosttyTerminalView.swift:120-122`)
cancels `pendingGeometrySync` and nothing else. The
`ghostty_surface_t` allocated in `init` is leaked. Over a
long session that cycles workers through the 8-slot pool,
each completion leaks a surface (PTY fd + scrollback + GPU
resources).

### Options

- **(a)** Call `ghostty_surface_free(surface)` in `deinit`
and stop there.
- **(b)** Mirror upstream Ghostty.app's known teardown
sequence: clear focus, uninstall the cursor / tracking area,
optionally call `ghostty_surface_close` to let the surface
drain, then `ghostty_surface_free`.
- **(c)** Move teardown out of `deinit` entirely — call an
explicit `tearDown()` from `WorkersWorkspaceModel.releaseWorkerPane`
before nilling the session, and free the surface there.

### Discussion

`(a)` is the smallest possible patch. The risk is that
libghostty has invariants about callback ordering that
break if the surface is freed while focus is set or while a
tracking area still references the host view's `userdata`
pointer (passed via `Unmanaged.passUnretained(self).toOpaque()`
at init time). The PR #209 fix was specifically about a
use-after-free in the action callback path; it's plausible
the same pointer aliasing is dangerous in teardown.

`(b)` matches the canonical libghostty consumer (Ghostty.app
itself). The audit explicitly flags this as the question to
answer — see open question 1 in the audit.

`(c)` is more invasive (new call site, new method, error
paths in `releaseWorkerPane`) for not-clearly-better
guarantees. SwiftUI tearing the host view down via the
session-nil path already reaches `deinit` deterministically
on the main thread (the host view is `@MainActor` and
NSView lifecycle is main-thread).

### Recommendation

**Pick (b).** Concretely:

1. Before freeing, clear focus
   (`ghostty_surface_set_focus(surface, false)`) so libghostty
   stops considering this surface focused.
2. Invalidate the `claudeMonitorTimer` (already done by
   `viewDidMoveToWindow(window: nil)` but redundant call is
   safe and protects against deinit paths that bypass that
   hook).
3. Free the surface: `ghostty_surface_free(surface)` (or
   whatever the actual destructor name turns out to be in
   `GhosttyKit` headers — the audit grep found
   `ghostty_surface_free_text` only, which is the read-text
   helper, not the surface destructor; the implementation
   chore must verify against headers).

If the destructor is two-step (`close` then `free`), call
both in that order. Document the call ordering in a comment
referencing the upstream source we mirrored.

### Acceptance

- After `bossctl agents launch && bossctl agents stop` ×10,
`leaks $(pgrep BossMacApp)` reports no growth in
`ghostty_surface_t` allocations.
- `lsof -p $(pgrep BossMacApp) | grep -c PTY` is constant
across spawn/release cycles (modulo currently-active
workers).
- RSS does not creep across spawn/release cycles in a 10-cycle
loop.

---

## Design Question 2 — Boss / worker timeline accumulation

### Problem

`appendAssistantChunk` (`ChatViewModel.swift:1086-1101`),
`upsertTerminalActivity` / `appendTerminalOutput` /
`completeTerminalActivity` (`ChatViewModel.swift:1110-1145`)
keep extending `agents[i].timeline` and `agents[i].terminalEntryIndexByID`.
Nothing renders these for any agent today:

- Boss pane: rendered as a libghostty surface
(`BossPaneTerminalView`) — the legacy `messageList` helper
(`ContentView.swift:359`) is dead code and never called.
- Worker panes: also libghostty — `WorkerSlotView` shows
the libghostty `GhosttyTerminalView`, never an
`Agent.timeline`.

So every chunk and tool-output event mutates a
`@Published [Agent]`, fires a publisher update, invalidates
`ContentView`, and stores data nothing reads. The arrays
grow linearly with session length.

### Options

- **(a)** Skip accumulation only for the Boss agent
(`agent.isBoss`); leave it for worker agents.
- **(b)** Remove `Agent.timeline` and
`Agent.terminalEntryIndexByID` from the model entirely.
Drop `appendAssistantChunk`, the terminal activity
helpers, the `messageList` helper, `MessageBubble`, and
`TerminalActivityCard` if those latter views have no
other callers.
- **(c)** Keep accumulation but cap timelines (FIFO eviction
at N entries) so memory stays bounded even though no view
reads them.

### Discussion

`(a)` is a one-line change but leaves the dead-code path
present for workers. Workers don't render `timeline` either
— the audit confirms that `WorkersDetailView` only renders
the libghostty pane. Keeping the accumulation alive for
workers means we keep paying the per-chunk publisher cost
on `ChatViewModel` for every chunk event the engine emits,
for code nothing reads. That's the worst of both worlds.

`(c)` (capped FIFO) is what the audit's chore #7 suggests
as a fallback. It's the right shape if we wanted to keep
the timeline alive for some future feature (e.g., a
debug "show me what the worker said" pane). We don't have
that feature on the roadmap. Adding eviction logic for
data nothing reads is sandbag work.

`(b)` deletes the unused storage path entirely. The
remaining state on `Agent` (`isReady`, `isSending`,
`activeAssistantMessageID`) is still needed for status
display in the Agents tab (`isSending` drives the "Working…"
spinner; `isReady` gates the composer). The chunk handler
in `handle(_:)` becomes:

```swift
case .chunk(let agentId, _):
    mutateAgent(agentId) { $0.isSending = true }
```

i.e. just keeping the "sending" flag flipped while chunks
arrive. The `activeAssistantMessageID` field exists only to
let `appendAssistantChunk` find which timeline message to
append to; once timelines are gone, that field can go too.

### Recommendation

**Pick (b).** Delete `Agent.timeline`, `Agent.terminalEntryIndexByID`,
`Agent.activeAssistantMessageID`. Delete `appendAssistantChunk`,
`upsertTerminalActivity`, `appendTerminalOutput`,
`completeTerminalActivity`, `ensureTerminalActivity`,
`messageIndex`, `messageList`. Remove `MessageBubble` and
`TerminalActivityCard` after grepping for any other
references. Keep `bossTimeline` only if something *outside*
the deleted path consumes it — based on grep,
`bossTimeline` is referenced only by the deleted `messageList`
caller, so it goes too.

This collapses audit findings #5, #7, and #8 into one PR.

The system-message log path (`appendSystemMessage`) currently
appends to `agent.timeline` too. That path is gated on
`showSystemMessages` (the `BOSS_SHOW_SYSTEM_MESSAGES=1` env
var) and feeds nothing visible. Either delete it or change
it to `print(...)` to a debug log. Recommend `print` so the
env-gated diagnostic still produces output to stderr.

### Acceptance

- A 1-hour Boss session keeps `Agent` heap size constant —
verifiable via `heap $(pgrep BossMacApp) | grep " Agent "`
showing constant count + size.
- `chunk` and `terminal_*` events from the engine produce
zero `objectWillChange` fires on `ChatViewModel` if no
other state changes (the only update is `isSending`, which
flips once per turn rather than per chunk).
- `messageList`, `MessageBubble`, `TerminalActivityCard`
absent from `Sources/`.

---

## Design Question 3 — `worker.live_states` invalidation surface

### Problem

`workerLiveStatesByRunID` and `workerLiveStatesBySlot` are
both `@Published` on `ChatViewModel` (`ChatViewModel.swift:41,44`).
On every `worker_live_states_list` event the handler
reassigns both dictionaries from scratch
(`ChatViewModel.swift:820-826`). Two `@Published` writes →
two `objectWillChange` fires → entire
`ContentView.body` re-evaluates. The kanban (`workColumn`)
and toolbar / pickers all re-render even when the only
change is a per-slot `activity` flip.

### Options

- **(a)** Combine the two dictionaries into one
`@Published var liveStates: WorkerLiveStateMap` (a struct
holding both maps) so a single push fires one
`objectWillChange`, not two. Same invalidation surface.
- **(b)** Move both maps into a child `ObservableObject`
(`LiveWorkerStateStore`) that's owned by `ChatViewModel`
but observed only by views that need live state
(`WorkBoardCardView`, `WorkerSlotView`). `ContentView` and
the toolbar do not observe it.
- **(c)** Drop `@Published` entirely and emit a manual
`objectWillChange.send()` only when the *set* of slot ids
or per-slot `activity` actually changes (i.e., diff the
incoming list against the cached state).

### Discussion

`(a)` is the smallest fix and addresses the double-fire but
doesn't address the over-broad observer surface — every view
that observes `ChatViewModel` still re-renders on every
push, and pushes happen at hook-event cadence (multiple per
second during active work).

`(c)` is the most surgical but smells like we're
reimplementing what a properly-scoped observable already
gives us. The diff logic is also fiddly: we want to fire on
any `activity` change, any `currentTool` change, any
`lastEventAt` change… that's almost every push.

`(b)` is the right shape and matches the audit's "Note on
the published shape" callout. The kanban Doing card
(`WorkBoardCardView`) needs `WorkerLiveState` keyed by run
id; the slot row (`WorkerSlotView`) needs it keyed by slot
id. Both are leaf views that should observe a slim store
directly via `@ObservedObject` or `@StateObject`-on-parent.
`ContentView`, the work toolbar, the boss panel, and the
sidebar do not need to know about live state at all.

### Recommendation

**Pick (b).** Concretely:

1. Define `LiveWorkerStateStore: ObservableObject` with
two `@Published` maps (`byRunID`, `bySlot`) and an
`apply(_ states: [WorkerLiveState])` method that
recomputes both atomically (one
`objectWillChange.send()`).
2. `ChatViewModel` owns a `let liveWorkerStates = LiveWorkerStateStore()`.
The `workerLiveStatesList` event handler calls
`liveWorkerStates.apply(states)` instead of writing the
two `@Published` properties on the model.
3. `ContentView` (and any view that currently sees
`model.workerLiveStatesByRunID` only as a pass-through
to a child) injects the store down via `@EnvironmentObject`
or as a constructor parameter, not via the model. Leaf
views (`WorkBoardCardView`, `WorkerSlotView`) declare
`@ObservedObject var liveStates: LiveWorkerStateStore`
themselves.
4. Inside `apply`, short-circuit when the incoming state is
equal to the cached state (compare by
`Set(states)` — `WorkerLiveState` is `Hashable`). A
no-op push then fires no `objectWillChange` at all.

This addresses audit findings #4 and (combined with DQ2) #5
together — both were rooted in over-broad publisher
invalidation.

### Acceptance

- A `worker.live_states` push that produces the same set of
states as the previous push: zero `body` invalidations
anywhere in the app (verifiable with a
`Self._printChanges()` debug print on key views).
- A push that changes only one slot's `activity`: only the
`WorkerSlotView` for that slot and the `WorkBoardCardView`
for that run id re-render. `ContentView`, the toolbar,
the boss panel, the sidebar do not.

---

## Design Question 4 — Memoizing `visibleWorkItems` / `workSections`

### Problem

`visibleWorkItems` (`ChatViewModel.swift:112-142`) and
`workSections(in:)` (`ChatViewModel.swift:1224-1241`) are
computed properties / methods that walk all projects, all
tasks, all chores, sort, filter, and run
`localizedCaseInsensitiveContains` on every render. Every
column calls `workSections(in: column)` from its `body`,
once per column, every time `ContentView.body` runs. With
DQ3 in place the runs are rarer; without DQ3 it's per
engine event.

### Options

- **(a)** Cache the result in stored properties
(`cachedVisibleItems: [WorkTask]?`,
`cachedSectionsByColumn: [WorkBoardColumnKey: [WorkBoardSection]]?`)
invalidated on each setter that affects the inputs.
- **(b)** Compute on `@Published` writes via a Combine
pipeline: `$tasksByProjectID`, `$choresByProductID`,
`$selectedProjectFilterIDs`, `$workSearchText`,
`$includeChores`, `$showBlockedOnly`, `$selectedWorkProductID`
combined with `.combineLatest` and `.map`, debounced
slightly, materialised as an `@Published` derived state.
- **(c)** Move work items into an external
`WorkBoardStore: ObservableObject` that owns the inputs
and exposes `visibleItems` / `sectionsByColumn` as
`@Published` derived state, invalidated explicitly on each
mutation.

### Discussion

`(b)` is elegant on paper but Combine pipelines on
`@Published` properties of an `ObservableObject` are
notoriously order-sensitive (publisher fires *before* the
property is updated, not after, so `combineLatest` reads a
stale value). The fix is `.receive(on: DispatchQueue.main)`
+ `.sink` writing to a separate `@Published`, which is more
plumbing than `(a)` for the same end state.

`(c)` is a long-term refactor that splits the work surface
out of `ChatViewModel`. Same shape as DQ3's
`LiveWorkerStateStore`. It's the right destination but
much larger than this audit warrants.

`(a)` is the minimal fix: one cache, invalidated on every
setter that affects it. The setters are already the choke
points (`setSelectedWorkProductID`, `setSelectedProjectFilterIDs`,
the work-event handlers in the engine event switch, etc.).
Each one calls `invalidateWorkBoardCache()` after mutating
the input. `visibleWorkItems` becomes a function that
returns `cachedVisibleItems ?? recompute()`. Same for
`workSections(in:)` — cache per-column, invalidate on the
same triggers plus `setWorkBoardGrouping`.

### Recommendation

**Pick (a).** Add:

```swift
private var cachedVisibleItems: [WorkTask]?
private var cachedSectionsByColumn: [WorkBoardColumnKey: [WorkBoardSection]] = [:]

private func invalidateWorkBoardCache() {
    cachedVisibleItems = nil
    cachedSectionsByColumn.removeAll(keepingCapacity: true)
}
```

Hook `invalidateWorkBoardCache()` into every mutation that
changes an input — there are roughly a dozen call sites
(work-tree refresh, search text setter, filter toggles,
product change, grouping change, task move, task create /
edit / delete events). Convert `visibleWorkItems` from a
computed property to a function or memoize via the cache
inside its current shape. Same for `workSections(in:)`.

The cache invalidation is explicit, not magic. If a new
input is added later, the author has to call
`invalidateWorkBoardCache()` from their setter — same
hygiene as any explicit cache.

### Acceptance

- Scrolling the kanban (no input mutation, no engine
traffic) produces zero calls to the inner `visibleWorkItems`
recompute path. Verifiable with a `print` (deleted before
landing) or a simple counter property checked by a unit
test.
- A `worker.live_states` push that doesn't change any work
item produces zero `visibleWorkItems` recomputes (combined
with DQ3).
- Behavior identical to today: `WorkBoardCardView` /
column counts / drag-and-drop unaffected.

---

## Design Question 5 — Claude monitor screen-scrape lifecycle

### Problem

`startClaudeMonitor()` (`GhosttyTerminalView.swift:363-372`)
runs a 0.5 s timer per host view, scraping the viewport
text. With 8 worker panes plus the Boss pane, that's ~18
viewport reads per second on the main thread, indefinitely.
The `worker-live-status.md` design intends the engine's
`LiveWorkerState` to be authoritative; the screen-scrape
is a fallback for "before first hook event."

### Options

- **(a)** Cut the screen-scrape entirely. Show "Spawning"
until the engine sends the first `LiveWorkerState`.
- **(b)** Gate on `liveState`: start the timer in
`init`, stop it once a `LiveWorkerState` arrives for this
slot, restart if `LiveWorkerState` goes back to `nil`.
- **(c)** Drop the per-pane timer and run a single
shared 0.5 s timer at the workspace level that reads only
panes whose `liveState` is nil.

### Discussion

`(a)` is correct in steady state but loses the
before-first-hook fallback. The audit notes
`WorkersDetailView.swift:131-142` already prefers `liveState`
and falls back to `claudeState` only "until the worker's
first hook fires." Cutting the scrape means a 1–10 second
gap (cold spawn + first hook fire) where the pill says
"Spawning" with no information.

`(b)` keeps the same behavior but pays the cost only during
the cold-start window per pane. After the engine sends the
first `LiveWorkerState`, the timer stops and never restarts
(unless the engine connection drops and live state is
cleared, in which case we want the fallback again). This
needs the host view to observe `liveState` for its slot,
which means a back-channel from `WorkersWorkspaceModel` /
`LiveWorkerStateStore` to the host view, or a SwiftUI
`onChange` in the wrapping view that calls
`hostView.setClaudeMonitorEnabled(_:)`.

`(c)` reduces timer fan-out (1 timer instead of 9) but
doesn't reduce the work — the same N viewport reads run.
And once `(b)` is in place, only cold-start panes do the
work, which is at most one or two at a time. Adds
plumbing without payoff.

### Recommendation

**Pick (b).** Add `hostView.setClaudeMonitorActive(_:)`.
The `WorkerSlotView` body observes the slot's `liveState`
from `LiveWorkerStateStore` (introduced in DQ3) and calls
`setClaudeMonitorActive(false)` when `liveState != nil`,
`setClaudeMonitorActive(true)` when it goes back to `nil`.
Boss pane keeps the scrape always on (no `LiveWorkerState`
for the Boss pane today; if/when one is added, this gating
extends naturally).

### Acceptance

- With 8 active workers all reporting `LiveWorkerState`,
`sample BossMacApp 5` shows zero main-thread time in
`readVisibleContents` / `extractTail`.
- Status pill behavior unchanged: pill flips to engine
activity within one push of receiving the first
`LiveWorkerState`.

---

## Design Question 6 — `EngineClient` MainActor task batching

### Problem

`EngineClient.emit` (`EngineClient.swift:685-689`) creates a
`Task { @MainActor in onEvent?(event) }` per parsed event.
Continuous allocator pressure on `Boss.EngineClient` queue
during high-frequency `chunk` and `worker.live_states`
traffic.

### Options

- **(a)** `MainActor.assumeIsolated` when the queue is
already on main (it isn't — `Boss.EngineClient` is a
private dispatch queue), so this doesn't apply directly.
- **(b)** Coalesce by buffering events in a queue-local
array and flushing on a single MainActor hop per timer
tick (e.g., every 16 ms / per frame).
- **(c)** Defer entirely until DQ2 + DQ3 are landed, since
the per-event tax becomes much cheaper without the
downstream invalidation cascade.

### Discussion

DQ2 (timeline removal) and DQ3 (live-state store split)
together cut the *cost per event* on the MainActor side by
roughly the kanban + ContentView re-render. Once those land,
the per-event Task creation is the cheapest thing in the
event path; the audit ranks this finding LOW for that exact
reason.

`(b)` is correct but has subtle ordering implications: a
batch flush that interleaves `chunk` events from multiple
agents must preserve per-agent order. Once DQ2 lands, all
chunk events become `isSending = true` no-ops, so order
doesn't matter for them. For `worker.live_states_list`,
each push is idempotent given the latest state wins, so
the last one in a batch is the only one that matters.

### Recommendation

**Defer.** Land DQ2 and DQ3 first; re-measure with
Instruments; only do (b) if there's still measurable cost.
The audit ranks this LOW and we should respect that
ranking.

### Acceptance (if implemented)

- 1000 chunk events arriving in 1 second create O(10)
MainActor tasks rather than O(1000), measured by
`Instruments → Allocations → Task`.

---

## Phasing

Each phase is one PR.

**Phase 1 — quick wins, no behavior change.**

- DQ1 (libghostty surface free).
- TrekIcon caching (audit chore #3): static dictionary
keyed on `(character.rawValue, size.rawValue)`,
populated lazily, never evicted. Bounded at
9 × 4 = 36 entries. `MainActor`-isolated since the only
callers are `body` paths.

These are independent of each other and of every later
phase. Pick whichever is faster to verify.

**Phase 2 — kill dead accumulation paths.**

- DQ2 (timeline / terminalEntryIndexByID removal).
- This unblocks DQ6 by collapsing the per-chunk cost.

**Phase 3 — split the live-state observer surface.**

- DQ3 (`LiveWorkerStateStore` extraction).
- DQ5 (Claude monitor gating) — depends on the store
existing for slot-keyed observation.

**Phase 4 — kanban memoization.**

- DQ4 (`cachedVisibleItems` + per-column section cache).
- Independent of DQ2/DQ3 but the gain is much more visible
once they're in.

**Phase 5 — measure-then-decide.**

- DQ6 (engine event task batching) only if Instruments
still shows per-event allocator cost after Phase 1–4.
- Audit chore #9 (cancellable `DispatchQueue.main.async{After}`):
the unguarded scroll-pin path in `messageList`
(`ContentView.swift:386-391`) already goes away with DQ2;
the `TerminalOutputPane` path (`ContentView.swift:2009-2012`)
is the remaining one — small cleanup, fold into Phase 5
if Phase 5 is non-empty, otherwise skip.

---

## Risks and rollback

- **DQ1 wrong teardown order.** If we free the surface
while a callback is mid-flight, libghostty may crash.
Mitigation: implementation chore must verify against
`GhosttyKit` headers and Ghostty.app upstream before
landing; a panic on release is a very visible regression
and would be caught immediately on the next worker
spawn/release cycle. Rollback: revert the `deinit`
addition; re-introduces the leak but is otherwise safe.
- **DQ2 deletes a debug surface.** If anyone was using
`BOSS_SHOW_SYSTEM_MESSAGES=1` to see system messages in
the deleted boss panel, that signal goes away. Mitigation:
re-route `appendSystemMessage` to `print(...)` to stderr
so the env-gated diagnostic still produces output.
- **DQ3 changes object ownership.** Views that were
observing `model.workerLiveStatesBySlot` now have to
observe the new store. Compile-time errors will catch
every call site; no silent behavior change.
- **DQ4 stale cache.** A new mutator added later that
forgets to call `invalidateWorkBoardCache()` produces a
stale board. Mitigation: add a one-line `// IMPORTANT:
…invalidateWorkBoardCache()` comment on the cache fields;
add a unit test that calls each public mutator and
asserts the cache fields are nil afterward.
- **DQ5 cold-start regression.** If the gating logic is
backwards we leave the timer running forever, no worse
than today. If it stops the timer prematurely we render
"Spawning" longer than we should. Both are recoverable in
a follow-up patch.

## Open questions

- **Upstream Ghostty.app teardown source.** The audit's
open question 1 stands: confirm the exact destructor name
and ordering against `Ghostty.app/Sources/Ghostty/SurfaceView.swift`
or the equivalent before writing the DQ1 patch.
- **Live RSS curve vs predicted curve.** The audit infers
severities from code; an Instruments capture across a
30-minute session pre- and post-Phase 2 would let us
attribute actual MB/min to each fix. Worth doing as
fix-validation, not a precondition.
- **Boss agent live state.** Today `LiveWorkerState` is
worker-only. If/when the Boss pane gets one, DQ5's gating
logic generalises to the Boss pane and we delete the
"Boss pane keeps scrape always on" caveat. Out of scope
for this work.
