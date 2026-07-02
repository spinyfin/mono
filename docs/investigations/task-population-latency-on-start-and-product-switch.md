# Investigation: multi-second task-population delay on app start / product switch

**Status:** investigation complete — no code changed. Deliverable is this writeup plus ranked follow-up chores.
**Date:** 2026-07-01
**Execution:** `exec_18be5484b40c4f68_123`
**Scope:** measure and attribute the wall-clock of populating the kanban board (lanes, nav, counts) on (a) cold app start and (b) product switch, then propose remediations grounded in the measurements.

---

## TL;DR

- Populating the whole board for a product is **one RPC**: `GetWorkTree { product_id }` → one `WorkTree` reply. There is **no app-issued per-task N+1**.
- Inside that one RPC there **is a database N+1**: `get_work_tree` fans out to **~1,100–1,150 individual SQLite queries** for a 395-task product (2–3 per task in `collect_task_runtimes`, plus a per-doc-task loop).
- **But when the cache is warm, that N+1 costs only ~15–25 ms** on a reconstructed DB at the real cardinalities. Serialize (~2.5 ms), transport (<1 ms), and JSON decode (~2–15 ms) are also small. **The entire warm engine→wire→decode path is well under 100 ms — even doubled** (see below). So the warm path does **not** explain a _multi-second_ delay.
- The multi-second symptom is therefore dominated by the two segments this pass **could not instrument directly**: **(1) cold engine autostart** on cold app start (73 schema migrations + subsystem init at boot), and **(2) the app-side main-thread apply+render of ~400 SwiftUI cards** (317 of 395 are in the `done` lane) on both flows. There is **zero existing per-segment timing** on this path, so today you cannot read the breakdown off any log.
- Two concrete amplifiers make it worse: cold start issues **`GetWorkTree` twice** for the restored product, and the engine runs the blocking SQLite N+1 **on the tokio async thread with a strictly-serial per-connection dispatch loop**, so a slow tree build stalls the other ~7 cold-start requests behind it.
- **Highest-leverage first step is to add end-to-end timing instrumentation** (R1), precisely because the "obvious" culprit (the DB N+1) is only ~20 ms warm and optimizing it alone may not move the perceived delay. The batching/slimming/render fixes (R3–R6) then attack whichever segment the instrumentation confirms dominant.

---

## 1. Symptom and flows

When the Boss app launches, or when the user switches the selected product, there is a multi-second gap before tasks appear in the kanban lanes, the nav, and the counts. Two flows are in scope:

- **Cold app start** — app process launches, connects to (or spawns) the engine, and populates the initially-selected product's board.
- **Product switch** — engine already running; the user selects a different product and the board repopulates.

The distinction matters: only cold start pays engine-process startup. A product-switch delay must be explained _without_ engine boot, which sharply narrows its likely cause.

---

## 2. Methodology

### 2.1 What I could and could not touch

The live engine database (`~/Library/Application Support/Boss/state.db`) is **engine-owned and gated off** to worker/coordinator sessions — no `sqlite3`/`EXPLAIN` against real data, no reading the file. I therefore measured with two sanctioned surfaces:

1. **The `boss` CLI** (talks to the live engine over its socket) to read the **real workload cardinalities**.
2. **A synthetic SQLite database rebuilt from the repo's own schema** (`tools/boss/engine/core/src/work/schema_init.rs`) and seeded to those measured cardinalities, then driven with the **exact SQL the engine runs** (copied verbatim from the source). This is where the query-count, EXPLAIN QUERY PLAN, and timing numbers come from.

The macOS app and the Rust engine were **not built or run** in this pass (no build/run harness in the worker; the DB gate blocks live profiling anyway). App-side and engine-boot costs are therefore characterized _structurally_ (from the code, corroborated by three independent source-mapping passes) and bounded by reasoning, not timed. §8 lists every such gap explicitly.

### 2.2 Measurement harnesses

Three Python harnesses (SQLite via the stdlib `sqlite3` module — the same SQLite engine rusqlite links) reproduced under `scratchpad/`:

- `profile_worktree.py` — builds the synthetic DB (395 tasks, 40 projects, 564 executions, 763 runs, deps) and times the exact `get_work_tree` query sequence (N+1) vs. a batched alternative; runs `EXPLAIN QUERY PLAN` on the hot queries; measures serialized payload bytes and per-connection open cost.
- `profile_scaling.py` — tests whether the N+1 cost scales with **execution-history depth** vs. **task count**, and isolates the SQLite-page-cache-cold first pass.
- `profile_payload.py` — builds a realistic full `WorkTree` payload and times JSON encode/decode and payload size with/without `description`.

### 2.3 Proxy caveats (direction of error)

- **rusqlite is generally faster per query than Python's `sqlite3`** (no interpreter overhead per step). So the warm DB numbers below are a **conservative upper bound** for the real engine.
- **Swift `JSONSerialization` decode is typically slower than Python's C `json`.** So the decode numbers are a **lower bound** for the app.
- Timings are warm-cache and single-process; they isolate compute cost, not the true-cold-start disk and boot costs (which are exactly what §8 flags as unmeasured).

Numbers below were taken on this machine (Apple Silicon, macOS 15.3 / Darwin 25.3).

---

## 3. Workload characterization (real numbers, via `boss` CLI)

| Metric                                    | Value                                                 | Source                            |
| ----------------------------------------- | ----------------------------------------------------- | --------------------------------- |
| Products (active / total)                 | 3 / 4                                                 | `boss product list`               |
| Tasks on **Boss** product (the hot one)   | **395**                                               | `boss task list --product boss`   |
| — by status                               | done 317, todo 32, blocked 31, in_review 12, active 3 | same                              |
| — by kind                                 | project_task 331, design 50, investigation 14         | same                              |
| Tasks on Flunge                           | 24                                                    | `boss task list --product flunge` |
| Tasks on checkleft-sandbox / test-product | 0 / 0                                                 | same                              |

The dominant fact: **the Boss ("omni") product has 395 tasks, and 317 of them (80%) are in the `done` lane.** Every board load fetches, serializes, transports, decodes, and renders all 395 — the `done` history included. Switching _to_ the Boss product from a small product is the worst case.

Executions/runs per task could not be counted from the live DB directly; the synthetic DB models 1–3 executions per done/active task and 1–2 runs each (§8).

---

## 4. The path, end-to-end

Both flows converge on **one request**:

```
app  ──GetWorkTree{product_id}──▶  engine handler  ──▶  WorkDb::get_work_tree  ──▶  SQLite
app  ◀──────WorkTree{...}────────  serialize (serde_json)  ◀── assemble ◀────────  (N+1 queries)
app: decode (off-main) ─▶ group/sort/@Published apply (MAIN thread) ─▶ SwiftUI render ~400 cards
```

- Protocol: `FrontendRequest::GetWorkTree { product_id }` (`tools/boss/protocol/src/wire.rs:667`); reply `FrontendEvent::WorkTree { product, projects, tasks, chores, task_runtimes, dependencies }` (`wire.rs:1539`).
- Handler: `work_items::handle_get_work_tree` (`tools/boss/engine/core/src/app/work_items.rs:814`), dispatched at `app.rs:2377`.
- DB assembly: `WorkDb::get_work_tree` (`tools/boss/engine/core/src/work/workitems.rs:293`).
- App fetch triggers: `.connected` (`ChatViewModel.swift:1783`), `.productsList` (`:1888–1893`), `selectWorkProduct` (`:942`), `.workInvalidated` (`:1864`).
- Transport: **Unix-domain socket, newline-delimited (`\n`) framing, uncompressed UTF-8 JSON, one frame per message** (`client/src/lib.rs:112`; Swift `EngineClient.swift:344,1236,1283`). No length prefix, no chunking, no compression.
- Decode: **off the main thread** on a private serial queue (`EngineClient.swift:318`), via `JSONSerialization` + manual dictionary walking.
- Apply + render: `@MainActor ChatViewModel.handle(.workTree)` (`ChatViewModel.swift:1898`) replaces the product's buckets wholesale and sorts several times; SwiftUI then renders the lanes.

Key structural properties:

- **Subscriptions are invalidation-only.** `TopicEventPayload::WorkInvalidated { reason, product_id, item_ids }` (`wire.rs:2571`) is a _cache-bust signal, not data_; the app responds with a **full** `GetWorkTree` refetch (`ChatViewModel.swift:1864`). Nothing is streamed or delta'd.
- **Product switch is a full refetch + wholesale replace**, no diffing (`ChatViewModel.swift:928–946`, `:1904–1936`).
- **No git/gh/network in the hot path.** `resolve_task_doc_pointer` is passed `|_| None` for the workspace lookup and its URL helpers are pure string builders (`products_design.rs:705`); `gh` is only used by `create_revision`, not the read path.

---

## 5. Measurements and dominant-cost breakdown

### 5.1 The database N+1 (measured on the synthetic DB, warm cache)

`get_work_tree` issues a fixed handful of list queries **plus** a per-work-item fan-out:

- `collect_task_runtimes` (`dispatch_helpers.rs:231`) loops over **every task + chore** and runs, per item: `query_latest_execution_for_work_item`; then (if the latest row isn't `running`/`waiting_human` — true for all 317 done + todo + blocked + review) `query_live_execution_for_work_item`; then (if an execution exists) `query_latest_run`. That is **2–3 queries × 395 items**.
- A second loop resolves `resolve_task_doc_pointer` for each design/investigation (per-task-doc) item — ~2–3 queries × ~64 items (`workitems.rs:363`).

Measured, for the 395-task product:

| Path                                                                   | Queries   | warm p50    | warm p95 |
| ---------------------------------------------------------------------- | --------- | ----------- | -------- |
| **N+1 (as shipped)**                                                   | **1,131** | **24.6 ms** | 25.3 ms  |
| Batched alternative (window/GROUP BY: "latest exec + run per product") | 7         | 6.4 ms      | 6.8 ms   |

The N+1 cost is **bound by task count, not history depth** — deepening the execution history 8× barely moves it (each point query stays index-bound):

| executions / runs seeded | queries | warm p50 |
| ------------------------ | ------- | -------- |
| 332 / 664                | 1,108   | 12.5 ms  |
| 966 / 1,932              | 1,108   | 12.9 ms  |
| 2,551 / 5,102            | 1,108   | 13.9 ms  |

SQLite-cache-cold first pass (OS cache still warm): p50 13.6 ms / p95 24 ms — only marginally slower than warm. **A true cold OS-page-cache pass is where ~1,131 separate B-tree descents would hurt most, and that is exactly what could not be measured here (§8).**

### 5.2 EXPLAIN QUERY PLAN (synthetic DB)

| Query                          | Plan                                                               | Verdict                                                                                               |
| ------------------------------ | ------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------- |
| list tasks for product         | `SCAN tasks` + `USE TEMP B-TREE FOR ORDER BY`                      | ORDER BY `COALESCE(ordinal,0), created_at` is not index-served → temp sort. **Harmless at 395 rows.** |
| latest execution per work item | `SEARCH work_executions USING INDEX work_executions_work_item_idx` | Index-served; fast **per query** — the problem is the _count_.                                        |
| latest run per execution       | `SEARCH work_runs USING INDEX work_runs_execution_idx`             | Same.                                                                                                 |
| product dependencies           | `SCAN d` + 2 correlated scalar subqueries (each index-served)      | Fine at this scale.                                                                                   |

No large full-table scan, no missing index that matters at this scale, and **no WAL lock contention** — the read path opens a WAL snapshot (`schema_init.rs:377`, `journal_mode=WAL`, `busy_timeout=5s`), so the engine's writers do not block it. The bottleneck is the **number of round-trips**, not any single slow query.

### 5.3 Payload, serialize, transport, decode

Realistic full `WorkTree` payload (395 cards, description sizes modeled by kind):

| Measurement                                      | Value                           |
| ------------------------------------------------ | ------------------------------- |
| Full payload (with `description`)                | **827 KiB**                     |
| Payload without `description`                    | 489 KiB (**−41%**)              |
| `description` share of payload                   | 47%                             |
| Serialize (serde proxy) p50 / p95                | 2.5 / 2.6 ms                    |
| Decode (Swift proxy — **lower bound**) p50 / p95 | 2.2 / 2.4 ms                    |
| Unix-socket transport of 827 KiB @ ~1 GB/s       | ~0.85 ms (bandwidth negligible) |

### 5.4 The reframe

Summing the **measured warm** segments:

| Segment                                    | Warm cost                              |
| ------------------------------------------ | -------------------------------------- |
| DB `get_work_tree` (N+1, as shipped)       | ~15–25 ms                              |
| serialize (serde)                          | ~2.5 ms                                |
| transport (bandwidth)                      | <1 ms                                  |
| decode (Swift, ≥)                          | ~2–15 ms                               |
| **Total measured warm engine→wire→decode** | **~25–45 ms** (≤ ~100 ms even doubled) |

**This is the crux honest finding: the warm, measurable path is well under 100 ms — an order of magnitude short of "multi-second."** So the perceived delay is not explained by the DB N+1, serialization, transport, or decode when caches are warm.

---

## 6. Where the seconds actually go (attribution + amplifiers)

Given §5.4, the residual seconds live in segments this pass could not time directly. Ranked by likelihood:

1. **App-side main-thread apply + render (both flows — strongest candidate for product switch).** Decode is off-main, but `handle(.workTree)` runs on `@MainActor` and, in one synchronous burst, evicts+rebuilds the product's buckets and sorts them several times (`ChatViewModel.swift:1903,1930,1932`; `computeVisibleWorkItems` `:696`; `workItems(in:)` `:2681`) — repeated O(n log n) over ~400 items — _then_ SwiftUI builds and lays out the lanes. **317 of 395 cards are in the `done` lane.** If the lanes aren't virtualized, building ~400 card views synchronously is the most plausible multi-hundred-ms-to-second cost, and it applies to product switch (no engine boot). The app already ships a `MainThreadStallMonitor` that logs stalls >250 ms to `diagnostics/ui-stalls-*.jsonl` — **that log would confirm or refute this immediately on a live run** but was not available here.
2. **Cold engine autostart (cold start only).** On launch, if the socket is unreachable the app spawns the engine (`EngineProcessController`, `ChatViewModel.swift:1450`). Engine boot runs **73 schema migrations** (`schema_init.rs`, most no-ops on an up-to-date DB but still executed) plus subsystem init (pollers, sweeps, host registry, cube heartbeat) before it can serve `GetWorkTree`. Plausibly 1–3 s, entirely on the cold-start path, and invisible to product switch. **Not timed here.**
3. **True cold OS-page-cache DB reads (cold start).** The 1,131-query N+1 becomes materially more expensive when the DB pages aren't resident (each point query is a fresh set of B-tree descents from disk). Bounded by reasoning to tens–hundreds of ms for a few-MB DB, but the real `state.db` size and cold-read cost were not measurable.

Two amplifiers that make both worse and are cheap to confirm/fix:

- **Redundant double `GetWorkTree` on cold start (confirmed by reading the source).** With a product restored from `UserDefaults`, `.connected` fires `sendGetWorkTree(productID)` (`ChatViewModel.swift:1783`) and then `.productsList` fires `sendGetWorkTree(productID)` **again** for the same product (`:1892–1893`, the `else if let productID = currentSelectedProductID` branch). Two full tree builds back-to-back.
- **Blocking SQLite on the async runtime + strictly-serial dispatch.** `get_work_tree` runs synchronously inside the `async fn` handler with **no `spawn_blocking`** (`work_items.rs:825`), and the per-connection loop `.await`s each handler before reading the next request (`app.rs:2256`). So the ~1,131-query tree build blocks the tokio worker thread and stalls the other ~7 cold-start requests (`ListProducts`, `GetEngineHealth`, `GitHubAuthStatus`, live states, …) queued behind it — and the redundant second `GetWorkTree` doubles that.

### Existing instrumentation: none on this path

There is **no wall-clock timing** on `GetWorkTree`/`ListProducts` at any layer (confirmed across engine + app):

- `engine-trace.jsonl` is an event log with **no span durations**; `get_work_tree` emits **no trace line at all**.
- `ipc_log.rs` logs only the reverse _pane-control_ channel (`SpawnWorkerPane`, …), not `FrontendRequest`s, and carries only an epoch stamp, no `elapsed`.
- Metrics are **counters/gauges only** — no latency histograms.
- **No `#[instrument]`/`info_span!` anywhere**; `Instant::now()` appears only in background loops, never around a handler/query/serialize.
- Swift side: `UISignpost` covers only the Ghostty terminal panes; `designDocTimingLog` covers only the design-doc open path. The task board fetch/decode/render has no `Date()`/signpost timing. (`MainThreadStallMonitor` >250 ms is the one signal that would incidentally catch a render stall.)

You cannot currently read the cold-start/switch breakdown off any log — which is why R1 leads the remediations.

---

## 7. Ranked remediations

Each is tied to the measurement that justifies it. "Win" is relative to the _measured_ dominant cost; where the dominant cost is unmeasured (render/boot), the remediation is ranked by expected leverage and flagged as gated on R1.

| #      | Remediation                                                                                                                                                                                                                                                                                                                                                                          | Expected win                                                                                                                       | Complexity            | Measured basis                                                                                                                       |
| ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------- | --------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| **R1** | **Add end-to-end per-segment timing.** Engine: `Instant` around `get_work_tree` (total + list-queries + runtime-N+1 + serialize), logged with `duration_ms`. App: `Date()`/`os_signpost` around send→receive→decode→main-thread-apply→first-render in `EngineClient.consumeLines` / `ChatViewModel.handle(.workTree)`; surface existing `MainThreadStallMonitor` hits for this path. | Unblocks correct prioritization; without it the ~20 ms-warm DB N+1 looks like the culprit but isn't.                               | S                     | §6: zero timing exists on this path; §5.4: warm path <100 ms proves the seconds are elsewhere.                                       |
| **R2** | **Kill the redundant double `GetWorkTree` on cold start.** In `.productsList`, only refetch when the selection actually changed; the restored product was already fetched on `.connected`.                                                                                                                                                                                           | Halves cold-start engine board work (and removes one full main-thread apply).                                                      | XS                    | §6 amplifier, confirmed at `ChatViewModel.swift:1783` + `:1892–1893`.                                                                |
| **R3** | **Batch `collect_task_runtimes` (and the doc-pointer loop).** Replace the per-task fan-out with one windowed/`GROUP BY` "latest execution + latest run per work item for this product" query (mirror the existing `attach_ai_reviewing_flag` `IN(...)` pattern).                                                                                                                     | Measured **1,131 → 7 queries; 24.6 → 6.4 ms warm**, and a far larger win on a cold OS cache (removes ~1,000 cold B-tree descents). | S–M                   | §5.1 harness.                                                                                                                        |
| **R4** | **Slim the board payload.** Stop serializing `description` (and other card-irrelevant fields) in the `WorkTree` tasks; fetch description lazily when a card opens (the on-demand `loadExecutions`/`loadTranscript` pattern already exists).                                                                                                                                          | Measured **827 → 489 KiB (−41%)**; less decode + main-thread memory churn.                                                         | S–M (protocol change) | §5.3.                                                                                                                                |
| **R5** | **Incrementally populate + virtualize the `done` lane.** Render the active lanes (todo/doing/blocked/review) first; lazy-load / paginate `done` (it's history). Confirm the lanes use `LazyVStack`.                                                                                                                                                                                  | Attacks the strongest product-switch candidate: 317/395 = **80%** of cards are `done`.                                             | M–H                   | §3 (80% done); §6 #1. Gated on R1 to confirm render dominance.                                                                       |
| **R6** | **Move blocking SQLite off the tokio thread (`spawn_blocking`) + add a small read-connection pool.** Stops the tree build stalling the other cold-start requests; pool avoids per-call file-open + PRAGMA.                                                                                                                                                                           | Improves cold-start concurrency (7 requests currently serialize behind the slow one).                                              | S–M                   | §6 amplifier (`work_items.rs:825` no `spawn_blocking`; `app.rs:2256` serial; `schema_init.rs:377` fresh connection/PRAGMA per call). |
| **R7** | **Snapshot-then-delta protocol.** Replace "invalidate → full refetch" with a delta push keyed on the `item_ids` that `WorkInvalidated` already carries; send counts/active lanes first. Correct invalidation only — no stale-serving.                                                                                                                                                | Removes full refetch on every invalidation and every switch.                                                                       | H                     | §4 (invalidation-only subscription); §6 #1. Gated on R1.                                                                             |
| **R8** | **Reduce engine boot cost** (cold start only): gate the 73-migration pass behind the `schema_version` check so an up-to-date DB skips it; keep the engine resident so cold boot is rare.                                                                                                                                                                                             | Attacks cold-start-only seconds.                                                                                                   | M                     | §6 #2. **Gated on R1** — do not touch until boot time is actually measured.                                                          |

**Explicitly not worth doing** (measured to be non-issues): adding an index for the list `ORDER BY` (the temp-B-tree sort is negligible at 395 rows, §5.2); "fixing" WAL lock contention (there is none — readers snapshot, §5.2); compressing the wire (transport bandwidth is <1 ms, §5.3 — decode/render, not bytes/s, is the cost).

**Suggested sequencing:** R1 → R2 (both cheap, and R1 makes everything else measurable) → then whichever of R3/R4/R5 the R1 data shows dominant. R3+R4 are safe wins regardless (they reduce cold-cache and payload cost with low risk). R6 is a good hygiene fix bundled with R3. R5/R7/R8 are larger and should wait for R1's numbers.

---

## 8. What was NOT measured (honest baseline)

- **The live `state.db`.** Engine-owned and gated to worker sessions — no `sqlite3`/`EXPLAIN`/read against real data. All DB numbers are from a **synthetic DB rebuilt from the repo schema and seeded to CLI-measured cardinalities**. Real per-task execution/run counts, description-size distribution, soft-deleted-row volume, and WAL size are **approximated**, not observed.
- **True cold OS-page-cache cost.** Could not evict the OS cache (no sudo) or read the live file, so the disk-read component of cold start is **bounded by reasoning, not timed** — and it is exactly where the 1,131-query N+1 is most expensive.
- **Engine autostart / boot wall-clock.** The engine was not built or run; the 73-migration + subsystem-init cost is identified structurally but **not timed**.
- **App-side render / main-thread apply wall-clock.** The macOS app was not built or run (no harness in the worker). The synchronous group/sort burst + SwiftUI render of ~400 cards is identified structurally (three source passes) but **not timed**. A live run's `MainThreadStallMonitor` (`diagnostics/ui-stalls-*.jsonl`) would settle this immediately.
- **Absolute engine vs. proxy overhead.** Timings use Python `sqlite3` (DB — rusqlite is faster, so warm DB numbers are a **conservative upper bound**) and Python `json` (decode — Swift `JSONSerialization` is slower, so decode numbers are a **lower bound**).
- **Concurrent-writer contention under real dispatcher load.** WAL gives readers a snapshot and the source shows no read-side locking, but this was **not stress-tested** against a live engine mid-dispatch.

The single highest-value follow-up (R1) exists precisely to close the first four gaps: with per-segment timing on a live run, the true dominant cost becomes readable and R3–R8 can be prioritized against real numbers instead of the structural inference this pass had to rely on.

---

## Appendix A — measurement harnesses

Reproducible standalone Python scripts (no repo build required; stdlib `sqlite3`):

- `profile_worktree.py` — synthetic DB + N+1 vs batched timing + EXPLAIN QUERY PLAN + payload bytes + connection-open cost.
- `profile_scaling.py` — N+1 cost vs history depth; cold-SQLite-cache first pass; full path incl. serialize.
- `profile_payload.py` — full `WorkTree` payload size and encode/decode timing, with/without `description`.

They reconstruct the schema from `tools/boss/engine/core/src/work/schema_init.rs` and run the SQL verbatim from `work/workitems.rs` and `work/dispatch_helpers.rs`. (Kept in the investigation scratchpad, not committed — see the follow-up chore to land the equivalent as an engine bench.)

## Appendix B — key source references

- RPC/protocol: `tools/boss/protocol/src/wire.rs:667` (GetWorkTree), `:1539` (WorkTree reply), `:2571` (WorkInvalidated); `types.rs:2735` (Task, `description` at `:2786`), `:3042` (TaskRuntime).
- Engine handler + dispatch: `tools/boss/engine/core/src/app/work_items.rs:814`; `app.rs:2256` (serial loop), `:2377` (dispatch).
- DB assembly + N+1: `work/workitems.rs:293` (get_work_tree), `:316` (task list SQL), `:363` (doc-pointer loop); `work/dispatch_helpers.rs:231` (collect_task_runtimes), `:239–341` (per-item exec/run queries), `:203` (collect_product_dependencies); `work/revision_helpers.rs:336` (attach_ai_reviewing_flag — the batched contrast); `work/products_design.rs:705` (resolve_task_doc_pointer); `work/schema_init.rs:53` (tasks DDL), `:82` (indexes), `:377` (connect/PRAGMA).
- App fetch/apply/render: `tools/boss/app-macos/Sources/ChatViewModel.swift:1783` / `:1892` (double fetch), `:928` (selectWorkProduct), `:1898` (workTree apply), `:696` (computeVisibleWorkItems), `:2681` (workItems(in:)); `EngineClient.swift:318` (decode queue), `:543` (sendGetWorkTree), `:1236/1283` (framing).
- Transport: `tools/boss/client/src/lib.rs:112`.
- Instrumentation (absence): `engine/core/src/main.rs:58` (engine-trace.jsonl), `ipc_log.rs`, `app/metrics.rs`; Swift `Diagnostics/UISignposts.swift`, `MainThreadStallMonitor`.
  </content>
  </invoke>
