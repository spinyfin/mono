# Attribute `specialized Collection<>.firstIndex(of:)` hot leaf — 2026-07-26

## Question

Is `specialized Collection<>.firstIndex(of:) (in Boss)` — 278 top-of-stack samples, the largest Boss-binary leaf in the Boss UI performance improvements baseline — the byte scan at `tools/boss/app-macos/Sources/EngineClient.swift:189` in `consumeLines()`? Which thread does it run on? Does that refute the distilled baseline’s placement inside `applyWorkTree` / `ChatViewModel`? Should design-doc entry 12 (receive-buffer fix) leave deferred?

## Method

- **Static analysis only** (this note). No Instruments / `sample` capture was taken on a live Boss.app; residual human capture is listed at the end.
- Enumerated every `firstIndex(of:)` call site under `tools/boss/app-macos/Sources/`.
- Distinguished `firstIndex(of:)` (Element equality — matches the Instruments leaf name) from `firstIndex(where:)` (predicate — different specialization; what `applyWorkTree` uses).
- Traced the receive call graph, queue ownership, and MainActor hop for `EngineClient`.
- Cross-checked against the design finding in `tools/boss/docs/designs/boss-ui-performance-improvements.md` (“Finding: the largest Boss-binary leaf is off-main…”) and entry 11/12 task text.

## Verdict (static)

**Confirmed (static confidence: high).** The only hot-path `firstIndex(of:)` in the Boss app binary is the newline byte scan in `EngineClient.consumeLines()`. It runs on the private serial queue `Boss.EngineClient`, **not** on the main thread, and **not** inside `applyWorkTree`. The remit’s / distilled baseline’s expectation that this leaf lives in `applyWorkTree` is **refuted** by source.

A confirming Instruments sample that names the leaf’s thread and frames is still residual (see below). Independent sample evidence already in the design doc attributes `consumeLines()` itself to 940 samples on `DispatchQueue_129: Boss.EngineClient`, which is consistent with this attribution even without a re-capture of the leaf alone.

## Call graph and thread

```
connection.start(queue: Boss.EngineClient)          // EngineClient.swift — private DispatchQueue(label: "Boss.EngineClient")
  └─ NWConnection receive completion (same queue)
       └─ receiveNext()                             // append chunk to buffer
            └─ consumeLines()                       // firstIndex(of: 0x0A) + removeSubrange + JSON parse
                 └─ emit(event)                     // Task { @MainActor in onEvent?(event) }
                      └─ ChatViewModel handlers
                           └─ applyWorkTree(...)    // MainActor only — no firstIndex(of:)
```

Evidence in source:

| Fact                                 | Location                                                                                 |
| ------------------------------------ | ---------------------------------------------------------------------------------------- |
| Private serial queue                 | `private let queue = DispatchQueue(label: "Boss.EngineClient")` — `EngineClient.swift:8` |
| NWConnection scheduled on that queue | `connection.start(queue: queue)` — `EngineClient.swift:100`                              |
| Receive → consume                    | `receiveNext` appends data then `self.consumeLines()` — `EngineClient.swift:170–172`     |
| The leaf call site                   | `buffer[searchStart...].firstIndex(of: 0x0A)` — `EngineClient.swift:189`                 |
| Decode explicitly off-main           | Comments at `EngineClient.swift:259–263` (work_tree path)                                |
| MainActor hop **after** parse        | `emit` is `Task { @MainActor in self.onEvent?(event) }` — `EngineClient.swift:924–928`   |

`applyWorkTree` (`ChatViewModel+WorkItemEvents.swift:14–106`) runs only after the MainActor hop. Its only nearby index lookup is `products.firstIndex(where: { $0.id == product.id })` in `upsertProduct` — a **predicate** specialization on a small product list, not `firstIndex(of:)` on `Data`/`UInt8`, and not a plausible source of 278 top-of-stack samples under a live work-tree flood.

## Why the symbol name matches EngineClient and not applyWorkTree

Instruments reports `specialized Collection<>.firstIndex(of:) (in Boss)`.

- The `(of:)` overload is **Element equality** (`Equatable` element, no closure).
- The `(where:)` overload is a different symbol (predicate closure). Almost every ChatViewModel / work-tree index lookup is `firstIndex(where:)`.
- The EngineClient site is `firstIndex(of: 0x0A)` on a `Data.SubSequence` of `UInt8`. Swift specializes that into the Boss binary (`(in Boss)`), matching the leaf label.
- Adjacent cost on the same path: `buffer.removeSubrange(...newline)` is an O(n) memmove per completed line — real, but a different symbol.

## Full `firstIndex(of:)` inventory (app Sources)

Ten call sites. Only one is on a continuous hot path.

| Site                                | Element / collection          | Hot?    | Why not the 278-sample leaf                                                     |
| ----------------------------------- | ----------------------------- | ------- | ------------------------------------------------------------------------------- |
| **`EngineClient.swift:189`**        | `UInt8` / `Data.SubSequence`  | **Yes** | Continuous per socket chunk; multi-MB `work_tree` lines arrive as ~64 KiB reads |
| `EngineProcessController.swift:209` | `UInt8` / `Data`              | No      | One-shot version-check probe                                                    |
| `EngineProcessController.swift:622` | `UInt8` / `Data`              | No      | One-shot shutdown RPC                                                           |
| `Models+Planner.swift:120`          | `Character` / `String`        | No      | Cold planner parse                                                              |
| `BossPaneModel.swift:359`           | `Character` / `String`        | No      | Env-line parse                                                                  |
| `GhosttyRuntime.swift:93`           | `Character` / `String`        | No      | Env-line parse                                                                  |
| `LogViewer.swift:48–49`             | order element                 | No      | Log sort UI                                                                     |
| `DeferredScopeAttentions.swift:42`  | `Character` / `String`        | No      | Cold string parse                                                               |
| `DispatchEventsData.swift:152`      | `Character` `"\n"` / `String` | Low     | Occasional event chunk; not the sustained work_tree flood                       |

None of these are on `[WorkTask]`. None sit inside `applyWorkTree`.

## Relationship to the earlier O(n²) framing fix

`consumeLines` already tracks `unscannedPrefixLength` so each new chunk only scans the unscanned tail (regression-tested by `EngineClientLargeMessageFramingTests`). That fixed the quadratic **re-scan of already-scanned bytes** when a large single-line message arrived in many chunks.

What remains (and what entry 12 targets):

1. A linear `firstIndex(of: 0x0A)` over each new unscanned region (still visible as the specialized leaf).
2. An O(n) `removeSubrange(...newline)` memmove of the remainder of the buffer **per completed line**.

So entry 12 is **not** re-doing the O(n²) fix; it is the follow-on “index-advancing reader that does not re-copy the buffer per line.”

## Sample numbers already on record (from design doc baseline)

| Symbol / region                                      | Samples          | Thread (as labeled in baseline)                                                     |
| ---------------------------------------------------- | ---------------- | ----------------------------------------------------------------------------------- |
| `specialized Collection<>.firstIndex(of:) (in Boss)` | 278 top-of-stack | Not leaf-attributed in distillation; design finding places it on EngineClient queue |
| `consumeLines()` inclusive                           | 940              | `DispatchQueue_129: Boss.EngineClient`                                              |
| `EngineClient.receiveNext()` inclusive               | 2114             | same EngineClient queue                                                             |
| `-[_NSJSONReader parseData:options:error:]`          | 939              | same path, off-main                                                                 |
| `applyWorkTree` inclusive                            | 1219             | MainActor / ChatViewModel path (separate stack)                                     |

These support “same queue, framing + decode dominate EngineClient CPU” without proving the leaf’s exact frames; the source inventory closes the gap statically.

## Recommendation on entry 12 (receive-buffer fix)

**Keep entry 12 deferred for the stated v1 goal (main-thread CPU), and promote it if / when open question 2 answers “total process CPU is in scope.”**

Rationale:

1. **Attribution holds (static):** the leaf is EngineClient framing, not applyWorkTree. Fixing it cannot move main-thread `sizeThatFits` / `WorkTask.==` / `AG::Graph` stacks.
2. **It is still real process CPU:** 278 top-of-stack samples as the largest Boss-binary leaf, nested under ~940 inclusive in `consumeLines` and ~2114 in `receiveNext`, on a laptop that the project description says sits at 25–90% CPU. Worth doing under a total-CPU goal.
3. **Promotion gate is already written:** design-doc open question 2 and entry 12 scope text both say promote if total process CPU is in scope. This investigation does **not** answer that product question; it only confirms the leaf is the off-main site those entries already named.
4. **Effort stays small** if promoted: replace scan + `removeSubrange` with an advancing read cursor / ring or front-index drain; existing large-message framing test is the regression net for not reintroducing O(n²).

Do **not** promote entry 12 solely because the baseline mis-attributed the leaf to applyWorkTree — that was a planning error about _where_ the cost lives, not evidence that the cost is on the main thread.

## Residual — human capture

An agent cannot launch Boss.app or take a controlled Instruments/`sample` window under load. To close the loop with a measured attribution:

1. Reproduce a busy board (large work_tree traffic, same spirit as the population-latency / UI-perf protocol).
2. Sample ~60s; symbolicate with Boss dSYM.
3. Confirm `specialized Collection<>.firstIndex(of:)` (or equivalent specialized name) stacks under `EngineClient.consumeLines` / `receiveNext` on a thread whose queue label is `Boss.EngineClient` (name may be `DispatchQueue_N: Boss.EngineClient`).
4. Confirm it does **not** appear under `applyWorkTree` / MainActor work stacks as the leaf’s primary parent.
5. Optionally note remaining `removeSubrange` / memmove share next to the scan.

Until that capture lands, treat this note as **static confirmation** with **independent supporting sample context** for the parent function (`consumeLines` @ 940 on EngineClient queue), not as a re-measured leaf.

## References

- Design plan finding + entries 11–12: `tools/boss/docs/designs/boss-ui-performance-improvements.md`
- Implementation: `tools/boss/app-macos/Sources/EngineClient.swift` (`consumeLines`, `receiveNext`, `emit`, queue)
- MainActor apply: `tools/boss/app-macos/Sources/ChatViewModel+WorkItemEvents.swift` (`applyWorkTree`)
- Framing regression: `tools/boss/app-macos/Tests/BossTests/EngineClientLargeMessageFramingTests.swift`
- Prior UI static audit (queue model): `tools/boss/docs/investigations/ui-performance-audit-2026-05-07.md`
