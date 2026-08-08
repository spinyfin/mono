# Work-board layout livelock (100% CPU beachball)

- **Date:** 2026-08-07
- **Subject:** Boss.app 1.0.502, pid 9837, macOS 26.5.2 (arm64), uptime ~28 h, RSS 820 MB
- **Artifacts (operator-local, not in the repo — this document is the durable record of their contents):**
  - `~/Library/Logs/boss-beachball-sample-2026-08-07.txt` — 5 s `sample` taken while wedged, 2817 main-thread samples
  - `~/Library/Logs/boss-hang-spindump-2026-08-07.txt` — Apple watchdog `hang` report, incident `696E2065-654B-43DD-85ED-E31DC33AE156`, `Duration: 269.40s`, `Note: Unresponsive for 268 seconds before sampling`
- **Related:** [`../designs/boss-ui-performance-improvements.md`](../designs/boss-ui-performance-improvements.md), [`ui-performance-audit-2026-05-07.md`](ui-performance-audit-2026-05-07.md)

## Verdict

The main thread was **livelocked, not deadlocked**. Zero of the 2817 main-thread samples are idle in `mach_msg_trap`; all of them are inside `__CFRunLoopDoObservers` → `NSHostingView.beginTransaction()` → `GraphHost.flushTransactions()`. `flushTransactions` drains a transaction queue that never empties, so the run loop never returns to waiting for events and no mouse or keyboard input is ever serviced. Nothing was blocking: process CPU over the hang window was 1.018 s of which the main thread alone was 1.000 s, and all 13 other threads were parked (`cf_release` had not run in 13443 s).

**Cause:** `WorkBoardCardView.onHover` wrapped its hover-state write in `withAnimation(.easeInOut(duration: 0.15))`. `withAnimation` sets `Transaction.animation` for the **entire update**, so it animates every animatable attribute that moves in that turn — including view _frames_ (`AnimatableFrameAttribute`), not just the card's brightness. Hover delivery runs at the _end_ of a graph update (`Update.dispatchActions()` → `EventBindingManager.enqueueHoverUpdateIfNeeded()`), so the transaction it joins is already carrying whatever layout changed that turn. Animated frames slide cards under a stationary pointer; the moved geometry produces the next hover transition; that transition opens the next global animated transaction. `AnimatorState.nextUpdate()` re-arms a frame on every tick, so there is always another transaction queued.

**Second drive edge:** `setDepBadgeHover` / `setRevisionBadgeHover` assigned their `@Published` highlight sets unconditionally. `WorkBoardSectionItemsView` observes the whole `ChatViewModel` and reads both sets, so a hover tick that changed nothing still fired `objectWillChange`, re-evaluated the column, rebuilt every card's `WorkCardSnapshot`, and re-applied the entire `LazyVStack` list — which invalidated the responder tree and enqueued the next hover update.

Both edges are closed loops that require no user input to sustain, which matches the reproduction shape: the app had run ~28 h and accumulated only ~111 min of CPU, so the spin latched long after launch rather than at startup.

## Measured attribution

Inclusive share of the 2817 main-thread samples, counted at the **outermost** occurrence per stack so recursion is not double-counted:

| Symbol / family                           | Samples | % of main thread |
| ----------------------------------------- | ------: | ---------------: |
| `LazyLayoutViewCache.updateItemPhases`    |     345 |            12.2% |
| `AnimatableFrameAttribute`                |     326 |            11.6% |
| `LazySubviewPlacements`                   |     256 |             9.1% |
| `ForEachList.applyNodes`                  |     252 |             8.9% |
| `StackLayout…sizeChildrenIdeally`         |     215 |             7.6% |
| `EventBindingManager.enqueueHoverUpdate…` |     214 |             7.6% |
| `ScrollViewUtilities.sizeThatFits`        |     197 |             7.0% |
| `ViewResponder.containsGlobalPoints`      |     142 |             5.0% |
| `LazyStack.measureEstimates`              |     136 |             4.8% |
| `ForEach.IDGenerator.makeID`              |      40 |             1.4% |
| `initializeWithCopy for WorkTask`         |      16 |             0.6% |
| `FlowLayout` (all frames)                 |       4 |             0.1% |
| `mach_msg` (idle)                         |       0 |             0.0% |

The layout families nest and must not be summed.

## Two hypotheses the numbers refute

Both were plausible from the stack text alone and both are wrong. Recording them here so they are not re-opened.

**`ForEach` deep-copying `WorkTask` for item identity is not the amplifier.** It is real — `ForEach.IDGenerator.makeID` → `Collection.subscript.read` → `Array.subscript.read` → `initializeWithCopy for WorkTask` is exactly what the stack shows, and `WorkTask` is a 53-field struct so the outlined copy is a long run of retains. It is also **1.1%** of the main thread. Note the mechanism is not the one the shape suggests: `WorkTask.id` is a stored `let id: String`, so nothing is forced by the `Identifiable` conformance's getter — the copy comes from `Array.subscript.read` handing `makeID` a whole element by value. Fixing it means giving `ForEach` a lighter element type, which is a real change to a hot, well-covered file for a ~1% win. Not done here, and it should not be sold as a livelock fix if it is done later.

**`FlowLayout` is not a contributor.** At 0.1% it is below noise. Checked anyway, since the brief asked: `sizeThatFits` **is** idempotent for repeated identical proposals (`rows(for:)` short-circuits on `cache.rowsMaxWidth == maxWidth` and `ensureSizes` only re-measures on a subview-count mismatch), and its `Cache` mutation during measurement is the documented `Layout` contract, not a bug. There is a small inefficiency — `sizeThatFits` with a `nil` proposal width caches rows against `.infinity`, and the following `placeSubviews` recomputes them for the finite `bounds.width` — but each computation is deterministic and O(n). It thrashes; it does not oscillate.

## Amplifiers (why one iteration is expensive, not why it never converges)

These make each pass of the loop cost enough to peg a core. They are not fixed here — fixing them is a layout restructure that this repo's own measurement protocol gates on a human before/after capture.

**The lazy stack re-measures its entire item list every pass.** Read outer-to-inner, the layout stack is: `GeometryReader` (`ContentView.swift:948`) → `ScrollView(.horizontal)` (`:959`) → `HStack` (`:960`, `sizeChildrenIdeally`) → column `.padding`/`.frame` (`:1025-1027`) → column `VStack` (`:978`, `sizeChildrenGenerallyWithConcreteMajorProposal` → `prioritize` → `lengthThatFits`) → `ScrollView(.vertical)` (`:1007`) → inner `VStack` (`:1008`, `sizeChildrenIdeally`) → `LazyVStack` (`WorkBoardCard.swift:56`) → `LazyStack.measureEstimates` → `ForEachList.applyNodes` over the full list. The column `VStack` has to ask the vertical `ScrollView` how much length it wants in order to distribute height among header/divider/scroll area, and answering that forces the `LazyVStack` to estimate over every item. Laziness applies to view instantiation, not to this measurement.

**`LazyLayoutViewCache` re-dirties the graph inside the transaction.** `GraphHost.runTransaction` → `LazyLayoutCacheItem.AllItemsPhaseMutation.apply()` → `updateItemPhases()` → `updateItemPhase(_:)` → `AG::Graph::value_set` → `propagate_dirty`. Committing item phases dirties graph nodes _during_ the transaction, which schedules another one. This is the mechanism by which an expensive pass becomes a non-terminating sequence of passes — but it only keeps firing because something upstream keeps moving the lazy stack's item geometry, which is what the animated frames were doing.

## Two stack readings worth correcting

- **"`LazyVStackLayout` nested in `LazyHStackLayout`" is a symbolication artifact, not a nested stack.** Both protocol witnesses forward to the same generic `LazyStack<>` implementation and appear as adjacent frames. There is one lazy stack on the board, not two.
- **The nested `ScrollView`s are real.** `ScrollViewLayoutComputer.Engine.sizeThatFits` genuinely appears twice in one stack with distinct intermediate frames, matching `ScrollView(.horizontal)` at `ContentView.swift:959` containing `ScrollView(.vertical)` at `:1007`. They are on **different axes**, so this is not the pathological same-axis case where each re-measures the other under an unconstrained proposal.

## Verifying the fix

The acceptance signal is a `sample` of the idle app showing the main thread parked in `mach_msg_trap` rather than spinning in `NSHostingView.beginTransaction()`. That capture needs a human at the machine with a populated board — see the "Who runs this" section of [`../designs/boss-ui-performance-improvements.md`](../designs/boss-ui-performance-improvements.md). Read `AnimatableFrameAttribute`, `LazyLayoutViewCache.updateItemPhases`, and `enqueueHoverUpdateIfNeeded` inclusive counts; all three should fall to near zero on an idle board, and `mach_msg` idle samples should dominate.
